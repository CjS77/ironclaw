use std::sync::Arc;

use crate::types::{
    INTAKE_ATTACHMENTS_MAX, INTAKE_PAGE_LIMIT_MAX, INTAKE_SCHEMA_VERSION, INTAKE_TEXT_MAX_BYTES,
    length_prefixed_sha256,
};
use crate::{
    IntakeError, IntakeItem, IntakeItemId, IntakeItemStatus, IntakeOwner, IntakePage, IntakeReader,
    IntakeRecorder, IntakeStatusTransition, ListIntakeItemsRequest, MarkChatHandledRequest,
    RecordIntakeItemRequest, RecordIntakeOutcome,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_extension_contracts::external::ExternalConversationRef;
use ironclaw_filesystem::{
    CasApply, CasUpdateError, ContentType, Entry, FilesystemError, FilesystemOperation, Filter,
    IndexKey, IndexKind, IndexName, IndexSpec, IndexValue, OrderedPage, OrderedQueryCursor,
    RootFilesystem, ScopedFilesystem, SortDirection, VersionedEntry, cas_update,
};
use ironclaw_host_api::{
    ids::ExtensionId, path::ScopedPath, product_adapter::AdapterInstallationId,
    resource::ResourceScope,
};

/// Every item lives directly under this directory of the owner's `/intake`
/// mount, named by its event-derived id.
const INTAKE_ITEMS_PREFIX: &str = "/intake/items";

const STATUS_KEY: &str = "status";
const CHAT_KEY: &str = "chat";
const RECEIVED_AT_KEY: &str = "received_at";
const ITEM_ID_KEY: &str = "item_id";

// No index carries the owner: every owner's items live under that owner's
// own per-user mount, so each query already reads exactly one owner's rows.
const RECEIVED_INDEX: &str = "intake_received_v1";
const STATUS_INDEX: &str = "intake_status_received_v1";
const CHAT_STATUS_INDEX: &str = "intake_chat_status_received_v1";
const RECEIVED_INDEX_KEYS: [&str; 2] = [RECEIVED_AT_KEY, ITEM_ID_KEY];
const STATUS_INDEX_KEYS: [&str; 3] = [STATUS_KEY, RECEIVED_AT_KEY, ITEM_ID_KEY];
const CHAT_STATUS_INDEX_KEYS: [&str; 4] = [CHAT_KEY, STATUS_KEY, RECEIVED_AT_KEY, ITEM_ID_KEY];

/// Items one owner-activity pass transitions per query.
const OWNER_ACTIVITY_BATCH: usize = 100;
/// Query rounds one owner-activity pass may run. Past this the pass fails
/// retryably rather than spinning: a vendor redelivery resumes the work.
const OWNER_ACTIVITY_MAX_ROUNDS: usize = 100;

const CURSOR_MAX_BYTES: usize = (20 + 1 + 32) * 2;

/// The filesystem-backed intake store.
///
/// Layout: one JSON record per item at `/intake/items/<item-id>.json` in the
/// owner's mount. The id derives from the event identity, so creating an item
/// is a create-if-absent CAS on its own path and a replay finds the existing
/// record. Listing and the owner-activity sweep read three declared ordered
/// projections. Nothing is ever deleted.
///
/// Owner isolation is the per-user mount. Every record also names its owner,
/// and the store checks it on every read path ([`ensure_owned`]): a row that
/// names another owner means the mount is misconfigured, and the operation
/// fails with [`IntakeError::ForeignRecord`] instead of hiding the row.
pub struct IntakeStore<F>
where
    F: RootFilesystem,
{
    filesystem: Arc<ScopedFilesystem<F>>,
}

impl<F> IntakeStore<F>
where
    F: RootFilesystem,
{
    pub fn new(filesystem: Arc<ScopedFilesystem<F>>) -> Self {
        Self { filesystem }
    }

    /// Declare the ordered projections before any write or query. Index
    /// declaration never backfills, so it must precede the first item; it is
    /// idempotent, and backends cache repeat declarations.
    async fn ensure_indexes(&self, scope: &ResourceScope) -> Result<(), IntakeError> {
        let prefix = items_prefix()?;
        for (name, keys) in [
            (RECEIVED_INDEX, RECEIVED_INDEX_KEYS.as_slice()),
            (STATUS_INDEX, STATUS_INDEX_KEYS.as_slice()),
            (CHAT_STATUS_INDEX, CHAT_STATUS_INDEX_KEYS.as_slice()),
        ] {
            let spec = IndexSpec::new(
                index_name(name)?,
                keys.iter()
                    .map(|key| index_key(key))
                    .collect::<Result<Vec<_>, _>>()?,
                IndexKind::Exact,
            );
            self.filesystem
                .ensure_index(scope, &prefix, &spec)
                .await
                .map_err(map_filesystem_error)?;
        }
        Ok(())
    }

    /// Run one ordered query. Every returned row keeps its place in the
    /// scan; a row whose record does not decode carries no item.
    async fn scan(
        &self,
        scope: &ResourceScope,
        filter: &Filter,
        page: &OrderedPage,
    ) -> Result<Vec<ScannedRow>, IntakeError> {
        let rows = self
            .filesystem
            .query_ordered(scope, &items_prefix()?, filter, page)
            .await
            .map_err(map_filesystem_error)?;
        rows.iter().map(scan_row).collect()
    }

    /// [`Self::scan`] for the read half, which declares nothing. The write
    /// paths declare the projections before an owner's first item, so an
    /// owner with no declared projection has never had an item: every
    /// backend answers that query `Unsupported`, and it reads as no rows.
    /// The filters and pages this store builds always fit its own
    /// projections, so `Unsupported` here means only "not declared yet".
    async fn scan_declared(
        &self,
        scope: &ResourceScope,
        filter: &Filter,
        page: &OrderedPage,
    ) -> Result<Vec<ScannedRow>, IntakeError> {
        match self
            .filesystem
            .query_ordered(scope, &items_prefix()?, filter, page)
            .await
        {
            Ok(rows) => rows.iter().map(scan_row).collect(),
            Err(FilesystemError::Unsupported {
                operation: FilesystemOperation::Query,
                ..
            }) => Ok(Vec::new()),
            Err(error) => Err(map_filesystem_error(error)),
        }
    }

    /// Move one item from `received` to `handled_by_owner`. Returns whether
    /// this call made the change.
    async fn mark_item_handled(
        &self,
        scope: &ResourceScope,
        id: &IntakeItemId,
        occurred_at: DateTime<Utc>,
    ) -> Result<bool, IntakeError> {
        let path = item_path(id)?;
        cas_update(
            self.filesystem.as_ref(),
            scope,
            &path,
            decode_item,
            encode_item,
            move |current: Option<IntakeItem>| async move {
                let Some(mut item) = current else {
                    return Err(IntakeError::Backend {
                        reason: "indexed intake item has no record".to_string(),
                    });
                };
                if item.status != IntakeItemStatus::Received {
                    return Ok(CasApply::no_op(item, false));
                }
                let at = occurred_at.max(item.received_at);
                item.status = IntakeItemStatus::HandledByOwner;
                item.status_history.push(IntakeStatusTransition {
                    status: IntakeItemStatus::HandledByOwner,
                    at,
                });
                Ok(CasApply::new(item, true))
            },
        )
        .await
        .map_err(map_cas_error)
    }
}

#[async_trait]
impl<F> IntakeRecorder for IntakeStore<F>
where
    F: RootFilesystem,
{
    async fn record(
        &self,
        request: RecordIntakeItemRequest,
    ) -> Result<RecordIntakeOutcome, IntakeError> {
        let scope = owner_scope(&request.owner);
        self.ensure_indexes(&scope).await?;
        let candidate = new_item(request);
        let path = item_path(&candidate.id)?;
        cas_update(
            self.filesystem.as_ref(),
            &scope,
            &path,
            decode_item,
            encode_item,
            move |current: Option<IntakeItem>| {
                let candidate = candidate.clone();
                async move {
                    let Some(existing) = current else {
                        return Ok(CasApply::new(
                            candidate.clone(),
                            RecordIntakeOutcome::Created(candidate),
                        ));
                    };
                    ensure_owned(&existing, &candidate.owner)?;
                    if same_event(&existing, &candidate) {
                        Ok(CasApply::no_op(
                            existing.clone(),
                            RecordIntakeOutcome::Duplicate(existing),
                        ))
                    } else {
                        Err(IntakeError::Conflict)
                    }
                }
            },
        )
        .await
        .map_err(map_cas_error)
    }

    async fn mark_chat_handled_by_owner(
        &self,
        request: MarkChatHandledRequest,
    ) -> Result<usize, IntakeError> {
        let scope = owner_scope(&request.owner);
        self.ensure_indexes(&scope).await?;
        let filter = Filter::And(vec![
            Filter::Eq {
                key: index_key(CHAT_KEY)?,
                value: IndexValue::Text(chat_key(
                    &request.extension_id,
                    &request.installation_id,
                    &request.chat_ref,
                )),
            },
            Filter::Eq {
                key: index_key(STATUS_KEY)?,
                value: IndexValue::Text(IntakeItemStatus::Received.as_str().to_string()),
            },
        ]);
        let mut handled = 0;
        let mut after = None;
        for _ in 0..OWNER_ACTIVITY_MAX_ROUNDS {
            // Oldest first, so the sweep stops at the first item that
            // arrived after the owner's reply. Each round continues after
            // the last row the previous one scanned, so a record that does
            // not decode (and so stays `received`) is passed, not revisited.
            let mut page = OrderedPage::new(
                index_name(CHAT_STATUS_INDEX)?,
                index_key(RECEIVED_AT_KEY)?,
                index_key(ITEM_ID_KEY)?,
                SortDirection::Ascending,
                OWNER_ACTIVITY_BATCH as u32,
            );
            if let Some(after) = after.take() {
                page = page.after(after);
            }
            // safety: each item transitions in its own CAS; the sweep is idempotent and resumable,
            // so a partial pass is completed by a redelivery rather than needing one transaction.
            let rows = self.scan(&scope, &filter, &page).await?;
            let exhausted = rows.len() < OWNER_ACTIVITY_BATCH;
            for row in &rows {
                let after_reply = match &row.item {
                    Some(item) => item.received_at > request.occurred_at,
                    None => row.received_at_micros > request.occurred_at.timestamp_micros(),
                };
                if after_reply {
                    return Ok(handled);
                }
                let Some(item) = &row.item else {
                    continue;
                };
                ensure_owned(item, &request.owner)?;
                if self
                    .mark_item_handled(&scope, &item.id, request.occurred_at)
                    .await?
                {
                    handled += 1;
                }
            }
            if exhausted {
                return Ok(handled);
            }
            after = rows.last().map(ScannedRow::position);
        }
        Err(IntakeError::Backend {
            reason: "owner activity exceeded its bounded sweep; a redelivery resumes it"
                .to_string(),
        })
    }
}

#[async_trait]
impl<F> IntakeReader for IntakeStore<F>
where
    F: RootFilesystem,
{
    async fn list(&self, request: ListIntakeItemsRequest) -> Result<IntakePage, IntakeError> {
        if request.limit == 0 || request.limit > INTAKE_PAGE_LIMIT_MAX {
            return Err(IntakeError::InvalidRequest {
                reason: "intake page limit is invalid",
            });
        }
        let after = request.cursor.as_deref().map(decode_cursor).transpose()?;
        let scope = owner_scope(&request.owner);
        // Reads declare no projection (a declaration is a write); see
        // `scan_declared`.
        let (index, filter) = match request.status {
            None => (index_name(RECEIVED_INDEX)?, Filter::All),
            Some(status) => (
                index_name(STATUS_INDEX)?,
                Filter::Eq {
                    key: index_key(STATUS_KEY)?,
                    value: IndexValue::Text(status.as_str().to_string()),
                },
            ),
        };
        // One extra row tells whether another page exists.
        let Ok(fetch) = u32::try_from(request.limit + 1) else {
            unreachable!("the page limit is checked against INTAKE_PAGE_LIMIT_MAX above")
        };
        let mut page = OrderedPage::new(
            index,
            index_key(RECEIVED_AT_KEY)?,
            index_key(ITEM_ID_KEY)?,
            SortDirection::Descending,
            fetch,
        );
        if let Some(after) = after {
            page = page.after(after);
        }
        let mut rows = self.scan_declared(&scope, &filter, &page).await?;
        let has_more = rows.len() > request.limit;
        rows.truncate(request.limit);
        // The cursor follows the last row scanned, decodable or not, so a
        // page of damaged records still moves the listing forward.
        let next_cursor = has_more
            .then(|| rows.last().map(ScannedRow::cursor))
            .flatten();
        let items: Vec<IntakeItem> = rows.into_iter().filter_map(|row| row.item).collect();
        for item in &items {
            ensure_owned(item, &request.owner)?;
        }
        Ok(IntakePage { items, next_cursor })
    }

    async fn get(
        &self,
        owner: &IntakeOwner,
        id: &IntakeItemId,
    ) -> Result<Option<IntakeItem>, IntakeError> {
        let scope = owner_scope(owner);
        let Some(versioned) = self
            .filesystem
            .get(&scope, &item_path(id)?)
            .await
            .map_err(map_filesystem_error)?
        else {
            return Ok(None);
        };
        let item = decode_item(&versioned.entry.body)?;
        ensure_owned(&item, owner)?;
        if item.id != *id {
            return Err(IntakeError::Serialization {
                reason: "intake record id does not match its path".to_string(),
            });
        }
        Ok(Some(item))
    }
}

/// One row of an ordered query: its place in the ordering, read from the
/// row's own index entries, and the item when its record decodes.
struct ScannedRow {
    received_at_micros: i64,
    id: IntakeItemId,
    item: Option<IntakeItem>,
}

impl ScannedRow {
    /// The opaque list cursor that resumes after this row.
    fn cursor(&self) -> String {
        encode_cursor(self.received_at_micros, &self.id)
    }

    /// The keyset position that resumes a query after this row.
    fn position(&self) -> OrderedQueryCursor {
        ordered_position(self.received_at_micros, &self.id)
    }
}

/// A record that does not decode (damaged, or written by a newer schema) is
/// skipped rather than failing the whole query: it is logged at debug by id
/// only, never with its content, and keeps its place in the scan.
fn scan_row(row: &VersionedEntry) -> Result<ScannedRow, IntakeError> {
    let unindexed = || IntakeError::Backend {
        reason: "ordered intake row lacks its index entries".to_string(),
    };
    let received_at_micros = match row.entry.indexed.get(&index_key(RECEIVED_AT_KEY)?) {
        Some(IndexValue::I64(micros)) => *micros,
        _ => return Err(unindexed()),
    };
    let id = match row.entry.indexed.get(&index_key(ITEM_ID_KEY)?) {
        Some(IndexValue::Text(id)) => IntakeItemId::new(id.as_str())?,
        _ => return Err(unindexed()),
    };
    let item = match decode_item(&row.entry.body) {
        Ok(item) => Some(item),
        Err(error) => {
            // `Display` of a decode error carries no record content.
            tracing::debug!(item_id = %id, %error, "skipping an intake record that does not decode");
            None
        }
    };
    Ok(ScannedRow {
        received_at_micros,
        id,
        item,
    })
}

fn new_item(request: RecordIntakeItemRequest) -> IntakeItem {
    let id = IntakeItemId::for_event(
        &request.extension_id,
        &request.installation_id,
        &request.external_event_id,
    );
    let (text, text_truncated) = bound_text(request.text);
    let mut attachments = request.attachments;
    let attachments_truncated =
        request.attachments_dropped || attachments.len() > INTAKE_ATTACHMENTS_MAX;
    attachments.truncate(INTAKE_ATTACHMENTS_MAX);
    IntakeItem {
        schema_version: INTAKE_SCHEMA_VERSION,
        id,
        owner: request.owner,
        extension_id: request.extension_id,
        installation_id: request.installation_id,
        external_event_id: request.external_event_id,
        chat_ref: request.chat_ref,
        contact_ref: request.contact_ref,
        text,
        text_truncated,
        attachments,
        attachments_truncated,
        received_at: request.received_at,
        status: IntakeItemStatus::Received,
        status_history: vec![IntakeStatusTransition {
            status: IntakeItemStatus::Received,
            at: request.received_at,
        }],
    }
}

/// Cut `text` to the byte bound on a character boundary.
fn bound_text(mut text: String) -> (String, bool) {
    let end = text.floor_char_boundary(INTAKE_TEXT_MAX_BYTES);
    let truncated = end < text.len();
    text.truncate(end);
    (text, truncated)
}

/// The store's one owner check, applied on every path that reads a record.
/// The per-user mount already keeps owners apart, so a mismatch is a mount
/// misconfiguration: fail closed rather than hide or disclose the row.
fn ensure_owned(item: &IntakeItem, owner: &IntakeOwner) -> Result<(), IntakeError> {
    if item.owner == *owner {
        Ok(())
    } else {
        Err(IntakeError::ForeignRecord)
    }
}

fn same_event(existing: &IntakeItem, candidate: &IntakeItem) -> bool {
    existing.extension_id == candidate.extension_id
        && existing.installation_id == candidate.installation_id
        && existing.external_event_id == candidate.external_event_id
}

fn validate_item(item: &IntakeItem) -> Result<(), IntakeError> {
    let invalid = |reason: &str| IntakeError::Serialization {
        reason: format!("intake item record is invalid: {reason}"),
    };
    if item.schema_version != INTAKE_SCHEMA_VERSION {
        return Err(invalid("unsupported schema version"));
    }
    if item.text.len() > INTAKE_TEXT_MAX_BYTES {
        return Err(invalid("text exceeds its bound"));
    }
    if item.attachments.len() > INTAKE_ATTACHMENTS_MAX {
        return Err(invalid("attachment list exceeds its bound"));
    }
    match item.status_history.last() {
        Some(last) if last.status == item.status => {}
        _ => return Err(invalid("status history does not end at the current status")),
    }
    if item.id
        != IntakeItemId::for_event(
            &item.extension_id,
            &item.installation_id,
            &item.external_event_id,
        )
    {
        return Err(invalid("id does not match the event identity"));
    }
    Ok(())
}

fn decode_item(bytes: &[u8]) -> Result<IntakeItem, IntakeError> {
    let item: IntakeItem =
        serde_json::from_slice(bytes).map_err(|error| IntakeError::Serialization {
            reason: error.to_string(),
        })?;
    validate_item(&item)?;
    Ok(item)
}

fn encode_item(item: &IntakeItem) -> Result<Entry, IntakeError> {
    validate_item(item)?;
    let body = serde_json::to_vec(item).map_err(|error| IntakeError::Serialization {
        reason: error.to_string(),
    })?;
    Ok(Entry::bytes(body)
        .with_content_type(ContentType::json())
        .with_indexed(
            index_key(STATUS_KEY)?,
            IndexValue::Text(item.status.as_str().to_string()),
        )
        .with_indexed(
            index_key(CHAT_KEY)?,
            IndexValue::Text(chat_key(
                &item.extension_id,
                &item.installation_id,
                &item.chat_ref,
            )),
        )
        .with_indexed(
            index_key(RECEIVED_AT_KEY)?,
            IndexValue::I64(item.received_at.timestamp_micros()),
        )
        .with_indexed(
            index_key(ITEM_ID_KEY)?,
            IndexValue::Text(item.id.as_str().to_string()),
        ))
}

fn owner_scope(owner: &IntakeOwner) -> ResourceScope {
    let mut scope = ResourceScope::system();
    scope.tenant_id = owner.tenant_id.clone();
    scope.user_id = owner.user_id.clone();
    scope
}

/// One chat's identity within one installation: the conversation route
/// without its per-message reply target.
fn chat_key(
    extension_id: &ExtensionId,
    installation_id: &AdapterInstallationId,
    chat_ref: &ExternalConversationRef,
) -> String {
    hex::encode(length_prefixed_sha256(&[
        extension_id.as_str(),
        installation_id.as_str(),
        &chat_ref.conversation_fingerprint(),
    ]))
}

fn items_prefix() -> Result<ScopedPath, IntakeError> {
    ScopedPath::new(INTAKE_ITEMS_PREFIX).map_err(|error| IntakeError::Backend {
        reason: error.to_string(),
    })
}

fn item_path(id: &IntakeItemId) -> Result<ScopedPath, IntakeError> {
    ScopedPath::new(format!("{INTAKE_ITEMS_PREFIX}/{id}.json")).map_err(|error| {
        IntakeError::Backend {
            reason: error.to_string(),
        }
    })
}

fn index_name(name: &str) -> Result<IndexName, IntakeError> {
    IndexName::new(name).map_err(|error| IntakeError::Backend {
        reason: format!("intake index name is invalid: {error}"),
    })
}

fn index_key(key: &str) -> Result<IndexKey, IntakeError> {
    IndexKey::new(key).map_err(|error| IntakeError::Backend {
        reason: format!("intake index key is invalid: {error}"),
    })
}

fn encode_cursor(received_at_micros: i64, id: &IntakeItemId) -> String {
    hex::encode(format!("{received_at_micros}:{}", id.as_str()))
}

fn decode_cursor(cursor: &str) -> Result<OrderedQueryCursor, IntakeError> {
    if cursor.is_empty() || cursor.len() > CURSOR_MAX_BYTES {
        return Err(invalid_cursor());
    }
    let bytes = hex::decode(cursor).map_err(|error| {
        tracing::debug!(%error, "rejected malformed intake cursor encoding");
        invalid_cursor()
    })?;
    let raw = String::from_utf8(bytes).map_err(|error| {
        tracing::debug!(%error, "rejected non-UTF-8 intake cursor");
        invalid_cursor()
    })?;
    let (micros, id) = raw.split_once(':').ok_or_else(invalid_cursor)?;
    let micros = micros.parse::<i64>().map_err(|error| {
        tracing::debug!(%error, "rejected intake cursor timestamp");
        invalid_cursor()
    })?;
    let id = IntakeItemId::new(id).map_err(|error| {
        tracing::debug!(%error, "rejected intake cursor item id");
        invalid_cursor()
    })?;
    Ok(ordered_position(micros, &id))
}

fn ordered_position(received_at_micros: i64, id: &IntakeItemId) -> OrderedQueryCursor {
    OrderedQueryCursor {
        value: IndexValue::I64(received_at_micros),
        tie_breaker: IndexValue::Text(id.as_str().to_string()),
    }
}

fn invalid_cursor() -> IntakeError {
    IntakeError::InvalidRequest {
        reason: "intake cursor is invalid",
    }
}

fn map_cas_error(error: CasUpdateError<IntakeError>) -> IntakeError {
    match error {
        CasUpdateError::Apply(error) => error,
        CasUpdateError::Timeout => IntakeError::Backend {
            reason: "intake CAS update timed out".to_string(),
        },
        CasUpdateError::RetriesExhausted => IntakeError::Backend {
            reason: "intake CAS retries exhausted".to_string(),
        },
        CasUpdateError::CasUnsupported => IntakeError::Backend {
            reason: "intake backend does not support CAS".to_string(),
        },
        CasUpdateError::Backend(error) => IntakeError::Backend {
            reason: error.to_string(),
        },
    }
}

fn map_filesystem_error(error: FilesystemError) -> IntakeError {
    IntakeError::Backend {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_within_the_bound_is_kept_whole() {
        assert_eq!(
            bound_text("hello".to_string()),
            ("hello".to_string(), false)
        );
    }

    #[test]
    fn text_over_the_bound_is_cut_before_a_split_character() {
        // A four-byte character straddling the bound backs off to its start.
        let text = format!("{}\u{1f600}tail", "a".repeat(INTAKE_TEXT_MAX_BYTES - 2));
        let (kept, truncated) = bound_text(text);
        assert!(truncated);
        assert_eq!(kept, "a".repeat(INTAKE_TEXT_MAX_BYTES - 2));
    }

    /// The chat key is persisted as an index value, so its derivation must
    /// never change: this pins the bytes.
    #[test]
    fn the_chat_key_derivation_is_stable() {
        let chat = ExternalConversationRef::new(None, "chat-1", None, None).expect("chat");
        assert_eq!(
            chat_key(
                &ExtensionId::new("acme-intake").expect("extension"),
                &AdapterInstallationId::new("acme-intake").expect("installation"),
                &chat,
            ),
            "a4902b3bd5aa33dd69a53f6d0ea9cdd578c7a76ca5264e2d1291fc179f2d4808"
        );
    }

    #[test]
    fn a_cursor_round_trips_and_rejects_garbage() {
        let id = IntakeItemId::new("0123456789abcdef0123456789abcdef").expect("id");
        let encoded = hex::encode(format!("42:{id}"));
        let decoded = decode_cursor(&encoded).expect("cursor");
        assert_eq!(decoded.value, IndexValue::I64(42));
        assert_eq!(decoded.tie_breaker, IndexValue::Text(id.into_inner()));
        assert!(decode_cursor(&hex::encode("42:../escape")).is_err());
        assert!(decode_cursor(&hex::encode("x:0123456789abcdef0123456789abcdef")).is_err());
    }
}
