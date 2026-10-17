use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_extension_contracts::external::{
    ExternalActorRef, ExternalConversationRef, ExternalEventId,
};
use ironclaw_host_api::ids::{ExtensionId, TenantId, UserId};
use ironclaw_host_api::product_adapter::AdapterInstallationId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::IntakeError;

/// Persisted schema version of one intake item record.
pub(crate) const INTAKE_SCHEMA_VERSION: u8 = 1;
/// Largest message text an intake item keeps. Longer text is cut on a
/// character boundary and the item is flagged `text_truncated`.
pub const INTAKE_TEXT_MAX_BYTES: usize = 64 * 1024;
/// Most attachment metadata entries one item keeps. Further entries are
/// dropped and the item is flagged `attachments_truncated`.
pub const INTAKE_ATTACHMENTS_MAX: usize = 32;
/// Largest page one list call returns. The `builtin.intake.list` tool, the
/// store's only consumer, offers exactly this bound, so a full page of
/// previews stays far inside the first-party output budget.
pub const INTAKE_PAGE_LIMIT_MAX: usize = 50;
const _: () = assert!(INTAKE_PAGE_LIMIT_MAX < u32::MAX as usize); // safety: compile-time only; a checked limit plus the look-ahead row fits a query page
const INTAKE_ATTACHMENT_ID_MAX_BYTES: usize = 512;
/// The most a bare RFC 6838 `type/subtype` can hold: 127 + 1 + 127.
const INTAKE_ATTACHMENT_MIME_MAX_BYTES: usize = 255;
/// RFC 6838 caps each of the type and subtype names at 127 characters.
const INTAKE_ATTACHMENT_MIME_NAME_MAX_BYTES: usize = 127;
const INTAKE_ATTACHMENT_FILE_NAME_MAX_BYTES: usize = 256;

const INTAKE_ITEM_ID_HEX_LEN: usize = 32;

/// Stable identity of one intake item.
///
/// Derived from `(extension, installation, external event id)`, so a replayed
/// webhook for the same event always names the same record: the item path is
/// the deduplication key, and creating it is a single create-if-absent CAS.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct IntakeItemId(String);

impl IntakeItemId {
    fn validate(value: &str) -> Result<(), IntakeError> {
        if value.len() != INTAKE_ITEM_ID_HEX_LEN
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(IntakeError::InvalidRequest {
                reason: "intake item id must be 32 lowercase hexadecimal characters",
            });
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, IntakeError> {
        let value = raw.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// The deterministic id of the item for one inbound event. Segments are
    /// length-prefixed so no two distinct triples share an encoding.
    pub fn for_event(
        extension_id: &ExtensionId,
        installation_id: &AdapterInstallationId,
        external_event_id: &ExternalEventId,
    ) -> Self {
        let digest = length_prefixed_sha256(&[
            extension_id.as_str(),
            installation_id.as_str(),
            external_event_id.as_str(),
        ]);
        let prefix: Vec<u8> = digest
            .iter()
            .take(INTAKE_ITEM_ID_HEX_LEN / 2)
            .copied()
            .collect();
        Self(hex::encode(prefix))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for IntakeItemId {
    type Error = IntakeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for IntakeItemId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IntakeItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<IntakeItemId> for String {
    fn from(id: IntakeItemId) -> Self {
        id.0
    }
}

/// The IronClaw user an intake item belongs to. Every read and mutation is
/// scoped by it; the store keeps each owner's items under that owner's own
/// mount.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IntakeOwner {
    pub tenant_id: TenantId,
    pub user_id: UserId,
}

/// Lifecycle of one intake item. Only states with a producer exist: an item
/// is `received` when stored, and `handled_by_owner` once the account owner
/// replies in the same chat from their own device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeItemStatus {
    Received,
    HandledByOwner,
}

impl IntakeItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::HandledByOwner => "handled_by_owner",
        }
    }
}

/// One status change, kept forever in the item's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeStatusTransition {
    pub status: IntakeItemStatus,
    pub at: DateTime<Utc>,
}

/// Metadata of one attachment the outsider sent. The bytes are not kept.
/// Every field except the size is outsider-controlled text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "IntakeAttachmentMetadataWire")]
pub struct IntakeAttachmentMetadata {
    attachment_id: String,
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
    size_bytes: u64,
}

#[derive(Deserialize)]
struct IntakeAttachmentMetadataWire {
    attachment_id: String,
    mime_type: String,
    #[serde(default)]
    file_name: Option<String>,
    size_bytes: u64,
}

impl TryFrom<IntakeAttachmentMetadataWire> for IntakeAttachmentMetadata {
    type Error = IntakeError;

    fn try_from(wire: IntakeAttachmentMetadataWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.attachment_id,
            wire.mime_type,
            wire.file_name,
            wire.size_bytes,
        )
    }
}

impl IntakeAttachmentMetadata {
    /// Validate adapter-supplied attachment metadata. The MIME type is a
    /// case-insensitive token and is normalized to lowercase here.
    pub fn new(
        attachment_id: impl Into<String>,
        mime_type: impl Into<String>,
        file_name: Option<String>,
        size_bytes: u64,
    ) -> Result<Self, IntakeError> {
        let attachment_id = attachment_id.into();
        let mime_type = mime_type.into().to_ascii_lowercase();
        if !bounded_text(&attachment_id, INTAKE_ATTACHMENT_ID_MAX_BYTES) {
            return Err(IntakeError::InvalidRequest {
                reason: "attachment id must be 1..=512 bytes without control characters",
            });
        }
        if !is_media_type(&mime_type) {
            return Err(IntakeError::InvalidRequest {
                reason: "attachment MIME type must be a bare type/subtype token",
            });
        }
        if let Some(name) = &file_name
            && !bounded_text(name, INTAKE_ATTACHMENT_FILE_NAME_MAX_BYTES)
        {
            return Err(IntakeError::InvalidRequest {
                reason: "attachment file name must be 1..=256 bytes without control characters",
            });
        }
        Ok(Self {
            attachment_id,
            mime_type,
            file_name,
            size_bytes,
        })
    }

    pub fn attachment_id(&self) -> &str {
        &self.attachment_id
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

/// A bare RFC 6838 media type: `type/subtype`, each a `restricted-name` of
/// 1..=127 characters (an ASCII letter or digit, then letters, digits, or
/// `! # $ & - ^ _ . +`), and no parameters. The grammar bounds the whole value
/// at 255 bytes ([`INTAKE_ATTACHMENT_MIME_MAX_BYTES`]).
///
/// Kept here rather than shared with the adapter contract's
/// `ProductAttachmentDescriptor` check: that one is private to the contracts
/// crate and looser (any `/`-bearing value up to 512 bytes), and making it a
/// public validator would put behaviour in a vocabulary crate. Every value
/// accepted here also passes that check.
fn is_media_type(value: &str) -> bool {
    fn restricted_name(name: &str) -> bool {
        let mut bytes = name.bytes();
        bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && name.len() <= INTAKE_ATTACHMENT_MIME_NAME_MAX_BYTES
            && bytes.all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+'
                    )
            })
    }
    value.len() <= INTAKE_ATTACHMENT_MIME_MAX_BYTES
        && value
            .split_once('/')
            .is_some_and(|(kind, subtype)| restricted_name(kind) && restricted_name(subtype))
}

/// SHA-256 over `segments`, each prefixed with its length as a big-endian
/// `u64`, so no two distinct segment lists share an encoding. The item id
/// and the chat key both derive from it, and both are persisted: changing
/// this encoding is a persisted-schema change.
pub(crate) fn length_prefixed_sha256(segments: &[&str]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for segment in segments {
        hasher.update((segment.len() as u64).to_be_bytes());
        hasher.update(segment.as_bytes());
    }
    hasher.finalize().into()
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

/// One stored inbound message from an outsider.
///
/// `text`, the chat and contact references, and every attachment field but
/// the size are third-party content. Readers that hand them to a model must wrap them in
/// the untrusted-content envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeItem {
    pub schema_version: u8,
    pub id: IntakeItemId,
    pub owner: IntakeOwner,
    pub extension_id: ExtensionId,
    pub installation_id: AdapterInstallationId,
    pub external_event_id: ExternalEventId,
    /// The chat the message arrived in.
    pub chat_ref: ExternalConversationRef,
    /// The sender. Its display name, when the message carried one, is the
    /// sender display name.
    pub contact_ref: ExternalActorRef,
    pub text: String,
    pub text_truncated: bool,
    pub attachments: Vec<IntakeAttachmentMetadata>,
    pub attachments_truncated: bool,
    pub received_at: DateTime<Utc>,
    pub status: IntakeItemStatus,
    pub status_history: Vec<IntakeStatusTransition>,
}

/// Store one verified inbound message. The store bounds the text and the
/// attachment list; callers pass what the adapter normalized.
#[derive(Debug, Clone)]
pub struct RecordIntakeItemRequest {
    pub owner: IntakeOwner,
    pub extension_id: ExtensionId,
    pub installation_id: AdapterInstallationId,
    pub external_event_id: ExternalEventId,
    pub chat_ref: ExternalConversationRef,
    pub contact_ref: ExternalActorRef,
    pub text: String,
    pub attachments: Vec<IntakeAttachmentMetadata>,
    /// Attachments the caller already dropped (for example because their
    /// metadata failed validation). Marks the item `attachments_truncated`.
    pub attachments_dropped: bool,
    pub received_at: DateTime<Utc>,
}

/// Result of [`IntakeRecorder::record`]. Both variants mean the event is
/// durably stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordIntakeOutcome {
    /// First delivery of this event: a new item exists.
    Created(IntakeItem),
    /// A replay of an event already stored. Nothing changed.
    Duplicate(IntakeItem),
}

impl RecordIntakeOutcome {
    pub fn item(&self) -> &IntakeItem {
        match self {
            Self::Created(item) | Self::Duplicate(item) => item,
        }
    }
}

/// Newest-first page of one owner's items, optionally filtered by status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListIntakeItemsRequest {
    pub owner: IntakeOwner,
    pub status: Option<IntakeItemStatus>,
    /// Opaque cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
    pub limit: usize,
}

/// One page of items. A stored record that does not decode is skipped but
/// still counts against the page, so `items` may be shorter than the limit
/// (even empty) while `next_cursor` is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakePage {
    pub items: Vec<IntakeItem>,
    pub next_cursor: Option<String>,
}

/// The account owner replied in `chat_ref` from their own device: mark that
/// chat's items received at or before `occurred_at` as handled by the owner.
#[derive(Debug, Clone)]
pub struct MarkChatHandledRequest {
    pub owner: IntakeOwner,
    pub extension_id: ExtensionId,
    pub installation_id: AdapterInstallationId,
    pub chat_ref: ExternalConversationRef,
    pub occurred_at: DateTime<Utc>,
}

/// The write half of the intake store, held by the intake channel sink in the
/// extension host: store verified inbound messages and move a chat's items
/// to `handled_by_owner`. Implemented by [`crate::IntakeStore`]; composition
/// hands the sink only this half.
#[async_trait]
pub trait IntakeRecorder: Send + Sync {
    /// Store one item, or return the existing one when the event was already
    /// stored.
    async fn record(
        &self,
        request: RecordIntakeItemRequest,
    ) -> Result<RecordIntakeOutcome, IntakeError>;

    /// Returns how many items moved to `handled_by_owner`.
    async fn mark_chat_handled_by_owner(
        &self,
        request: MarkChatHandledRequest,
    ) -> Result<usize, IntakeError>;
}

/// The read half of the intake store, held by the `builtin.intake.*` tools in
/// the host runtime. Implemented by [`crate::IntakeStore`]; composition hands
/// the tools only this half, so they cannot write.
#[async_trait]
pub trait IntakeReader: Send + Sync {
    async fn list(&self, request: ListIntakeItemsRequest) -> Result<IntakePage, IntakeError>;

    /// One item by id, or `None` when the owner holds no such item.
    async fn get(
        &self,
        owner: &IntakeOwner,
        id: &IntakeItemId,
    ) -> Result<Option<IntakeItem>, IntakeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (ExtensionId, AdapterInstallationId, ExternalEventId) {
        (
            ExtensionId::new("acme-intake").expect("extension"),
            AdapterInstallationId::new("acme-intake").expect("installation"),
            ExternalEventId::new("event-1").expect("event"),
        )
    }

    #[test]
    fn item_id_is_stable_per_event_and_differs_across_events() {
        let (extension, installation, event) = ids();
        let first = IntakeItemId::for_event(&extension, &installation, &event);
        assert_eq!(
            first,
            IntakeItemId::for_event(&extension, &installation, &event)
        );
        assert_eq!(first.as_str().len(), 32);
        let other = ExternalEventId::new("event-2").expect("event");
        assert_ne!(
            first,
            IntakeItemId::for_event(&extension, &installation, &other)
        );
        assert!(IntakeItemId::new(first.as_str()).is_ok());
    }

    /// The id names the record's path, so its derivation must never change
    /// (a replay of a stored event would otherwise create a second item).
    #[test]
    fn item_id_derivation_is_stable() {
        let (extension, installation, event) = ids();
        assert_eq!(
            IntakeItemId::for_event(&extension, &installation, &event).as_str(),
            "f932959ce8c3ab23d59aaea1556e6034"
        );
    }

    #[test]
    fn item_id_rejects_anything_but_lowercase_hex() {
        for bad in ["", "ABCDEF0123456789ABCDEF0123456789", "../etc", "0123"] {
            assert!(IntakeItemId::new(bad).is_err(), "{bad} must be rejected");
        }
        let wire = serde_json::from_str::<IntakeItemId>("\"../../escape\"");
        assert!(wire.is_err(), "wire decoding validates the id");
    }

    #[test]
    fn attachment_metadata_is_bounded_and_mime_is_normalized() {
        let metadata =
            IntakeAttachmentMetadata::new("a-1", "Image/PNG", None, 10).expect("valid metadata");
        assert_eq!(metadata.mime_type(), "image/png");
        assert!(IntakeAttachmentMetadata::new("", "image/png", None, 1).is_err());
        assert!(
            IntakeAttachmentMetadata::new("a", "image/png", Some("x\u{0}".to_string()), 1).is_err()
        );
        assert!(IntakeAttachmentMetadata::new("a", "image/png", Some("n".repeat(257)), 1).is_err());
    }

    #[test]
    fn mime_type_must_be_a_bare_rfc_6838_type_and_subtype() {
        for good in [
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "image/svg+xml",
            "audio/ogg",
            "x-custom/x.y_z-1!#$&^",
        ] {
            assert!(
                IntakeAttachmentMetadata::new("a", good, None, 1).is_ok(),
                "{good} is a valid MIME type"
            );
        }
        let longest = format!("{}/{}", "a".repeat(127), "b".repeat(127));
        assert!(IntakeAttachmentMetadata::new("a", longest.as_str(), None, 1).is_ok());
        for bad in [
            "<|im_start|>/system",
            "text/plain; charset=utf-8",
            "text/plain;charset=utf-8",
            "text",
            "text/",
            "/plain",
            "text/plain/extra",
            "-text/plain",
            "text/.plain",
            "text/pla\"in",
            "text/pl<a>in",
            "t\u{e9}xt/plain",
        ] {
            assert!(
                IntakeAttachmentMetadata::new("a", bad, None, 1).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let long_type = format!("{}/b", "a".repeat(128));
        assert!(IntakeAttachmentMetadata::new("a", long_type.as_str(), None, 1).is_err());
        let long_subtype = format!("a/{}", "b".repeat(128));
        assert!(IntakeAttachmentMetadata::new("a", long_subtype.as_str(), None, 1).is_err());

        let stored = serde_json::json!({
            "attachment_id": "a",
            "mime_type": "<|im_start|>/system",
            "size_bytes": 1,
        });
        assert!(
            serde_json::from_value::<IntakeAttachmentMetadata>(stored).is_err(),
            "a stored record is revalidated on decode"
        );
    }
}
