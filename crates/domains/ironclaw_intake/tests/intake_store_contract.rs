#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use ironclaw_extension_contracts::external::{
    ExternalActorRef, ExternalConversationRef, ExternalEventId,
};
use ironclaw_filesystem::{
    BackendCapabilities, CasExpectation, DirEntry, Entry, FileStat, FilesystemError, Filter,
    InMemoryBackend, IndexSpec, LibSqlRootFilesystem, OrderedPage, Page, RecordVersion,
    RootFilesystem, ScopedFilesystem, VersionedEntry,
};
use ironclaw_host_api::{
    ids::{ExtensionId, TenantId, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, ScopedPath, VirtualPath},
    product_adapter::AdapterInstallationId,
    resource::ResourceScope,
};
use ironclaw_intake::test_support::{in_memory_intake_store, per_user_intake_mount};
use ironclaw_intake::{
    INTAKE_ATTACHMENTS_MAX, INTAKE_PAGE_LIMIT_MAX, INTAKE_TEXT_MAX_BYTES, IntakeAttachmentMetadata,
    IntakeError, IntakeItemId, IntakeItemStatus, IntakeOwner, IntakeReader, IntakeRecorder,
    IntakeStore, ListIntakeItemsRequest, MarkChatHandledRequest, RecordIntakeItemRequest,
    RecordIntakeOutcome,
};
use tokio::sync::{Barrier, Mutex};

const EXTENSION: &str = "acme-intake";
const INSTALLATION: &str = "acme-intake";

fn owner(user: &str) -> IntakeOwner {
    IntakeOwner {
        tenant_id: TenantId::new("tenant-a").expect("tenant"),
        user_id: UserId::new(user).expect("user"),
    }
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn chat(id: &str) -> ExternalConversationRef {
    ExternalConversationRef::new(None, id, None, None).expect("chat ref")
}

fn request(
    owner: &IntakeOwner,
    event: &str,
    chat_id: &str,
    text: &str,
    received_at: DateTime<Utc>,
) -> RecordIntakeItemRequest {
    RecordIntakeItemRequest {
        owner: owner.clone(),
        extension_id: ExtensionId::new(EXTENSION).expect("extension"),
        installation_id: AdapterInstallationId::new(INSTALLATION).expect("installation"),
        external_event_id: ExternalEventId::new(event).expect("event"),
        chat_ref: chat(chat_id),
        contact_ref: ExternalActorRef::new("contact", "contact-1", Some("Pat Outsider"))
            .expect("contact"),
        text: text.to_string(),
        attachments: vec![
            IntakeAttachmentMetadata::new(
                "att-1",
                "image/png",
                Some("photo.png".to_string()),
                1234,
            )
            .expect("attachment"),
        ],
        attachments_dropped: false,
        received_at,
    }
}

fn list_request(owner: &IntakeOwner, limit: usize) -> ListIntakeItemsRequest {
    ListIntakeItemsRequest {
        owner: owner.clone(),
        status: None,
        cursor: None,
        limit,
    }
}

fn mark_request(
    owner: &IntakeOwner,
    chat_id: &str,
    occurred_at: DateTime<Utc>,
) -> MarkChatHandledRequest {
    MarkChatHandledRequest {
        owner: owner.clone(),
        extension_id: ExtensionId::new(EXTENSION).expect("extension"),
        installation_id: AdapterInstallationId::new(INSTALLATION).expect("installation"),
        chat_ref: chat(chat_id),
        occurred_at,
    }
}

#[tokio::test]
async fn a_replayed_event_returns_the_stored_item_instead_of_a_second_one() {
    let store = in_memory_intake_store();
    let alice = owner("alice");

    let first = store
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "hello",
            at(1_700_000_000),
        ))
        .await
        .expect("first delivery stores");
    let RecordIntakeOutcome::Created(created) = first else {
        panic!("first delivery must create an item");
    };
    assert_eq!(created.status, IntakeItemStatus::Received);
    assert_eq!(created.status_history.len(), 1);
    assert_eq!(created.status_history[0].status, IntakeItemStatus::Received);
    assert_eq!(created.text, "hello");
    assert!(!created.text_truncated);
    assert_eq!(created.attachments.len(), 1);
    assert_eq!(created.contact_ref.display_name(), Some("Pat Outsider"));

    // A replay may carry different bytes and a later receipt time; the stored
    // record is what the event means, and it is not rewritten.
    let replay = store
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "changed",
            at(1_700_000_500),
        ))
        .await
        .expect("replay is acknowledged");
    assert_eq!(replay, RecordIntakeOutcome::Duplicate(created.clone()));

    let page = store
        .list(list_request(&alice, 10))
        .await
        .expect("list after replay");
    assert_eq!(page.items, vec![created.clone()]);
    assert_eq!(
        store.get(&alice, &created.id).await.expect("get"),
        Some(created)
    );
}

#[tokio::test]
async fn text_and_attachments_are_bounded_without_splitting_a_character() {
    let store = in_memory_intake_store();
    let alice = owner("alice");
    // A four-byte character straddles the byte bound.
    let text = format!("{}😀tail", "a".repeat(INTAKE_TEXT_MAX_BYTES - 2));
    let mut long = request(&alice, "event-long", "chat-1", &text, at(1_700_000_000));
    long.attachments = (0..INTAKE_ATTACHMENTS_MAX + 3)
        .map(|index| {
            IntakeAttachmentMetadata::new(format!("att-{index}"), "text/plain", None, 1)
                .expect("attachment")
        })
        .collect();
    let item = store
        .record(long)
        .await
        .expect("long message stores")
        .item()
        .clone();
    assert!(item.text_truncated);
    assert!(item.text.len() <= INTAKE_TEXT_MAX_BYTES);
    assert_eq!(item.text, "a".repeat(INTAKE_TEXT_MAX_BYTES - 2));
    assert!(item.attachments_truncated);
    assert_eq!(item.attachments.len(), INTAKE_ATTACHMENTS_MAX);

    let mut dropped = request(&alice, "event-dropped", "chat-1", "x", at(1_700_000_001));
    dropped.attachments_dropped = true;
    let item = store.record(dropped).await.expect("stores").item().clone();
    assert!(item.attachments_truncated);
}

#[tokio::test]
async fn listing_is_newest_first_paginated_and_filterable_by_status() {
    let store = in_memory_intake_store();
    let alice = owner("alice");
    for (index, chat_id) in ["chat-1", "chat-2", "chat-1", "chat-2", "chat-1"]
        .iter()
        .enumerate()
    {
        store
            .record(request(
                &alice,
                &format!("event-{index}"),
                chat_id,
                &format!("message {index}"),
                at(1_700_000_000 + index as i64),
            ))
            .await
            .expect("record");
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .list(ListIntakeItemsRequest {
                owner: alice.clone(),
                status: None,
                cursor: cursor.clone(),
                limit: 2,
            })
            .await
            .expect("page");
        assert!(page.items.len() <= 2);
        seen.extend(page.items.iter().map(|item| item.text.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        seen,
        vec![
            "message 4",
            "message 3",
            "message 2",
            "message 1",
            "message 0"
        ]
    );

    let handled = store
        .mark_chat_handled_by_owner(mark_request(&alice, "chat-2", at(1_700_000_100)))
        .await
        .expect("mark");
    assert_eq!(handled, 2);
    let open = store
        .list(ListIntakeItemsRequest {
            owner: alice.clone(),
            status: Some(IntakeItemStatus::Received),
            cursor: None,
            limit: 10,
        })
        .await
        .expect("open items");
    assert_eq!(
        open.items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["message 4", "message 2", "message 0"]
    );
    let done = store
        .list(ListIntakeItemsRequest {
            owner: alice.clone(),
            status: Some(IntakeItemStatus::HandledByOwner),
            cursor: None,
            limit: 10,
        })
        .await
        .expect("handled items");
    assert_eq!(
        done.items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["message 3", "message 1"]
    );
}

#[tokio::test]
async fn invalid_limits_and_cursors_are_rejected() {
    let store = in_memory_intake_store();
    let alice = owner("alice");
    for limit in [0, INTAKE_PAGE_LIMIT_MAX + 1] {
        assert!(matches!(
            store.list(list_request(&alice, limit)).await,
            Err(IntakeError::InvalidRequest { .. })
        ));
    }
    for cursor in ["", "zz", "not-hex", &"ab".repeat(400)] {
        assert!(matches!(
            store
                .list(ListIntakeItemsRequest {
                    owner: alice.clone(),
                    status: None,
                    cursor: Some(cursor.to_string()),
                    limit: 5,
                })
                .await,
            Err(IntakeError::InvalidRequest { .. })
        ));
    }
}

#[tokio::test]
async fn owner_activity_marks_only_that_chats_earlier_open_items_and_keeps_history() {
    let store = in_memory_intake_store();
    let alice = owner("alice");
    let early = store
        .record(request(&alice, "e-1", "chat-1", "one", at(1_700_000_000)))
        .await
        .expect("record")
        .item()
        .clone();
    store
        .record(request(&alice, "e-2", "chat-1", "two", at(1_700_000_010)))
        .await
        .expect("record");
    let other_chat = store
        .record(request(
            &alice,
            "e-3",
            "chat-2",
            "elsewhere",
            at(1_700_000_005),
        ))
        .await
        .expect("record")
        .item()
        .clone();
    let later = store
        .record(request(
            &alice,
            "e-4",
            "chat-1",
            "after the reply",
            at(1_700_000_100),
        ))
        .await
        .expect("record")
        .item()
        .clone();

    let reply_time = at(1_700_000_050);
    let handled = store
        .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", reply_time))
        .await
        .expect("mark");
    assert_eq!(handled, 2, "only chat-1 items received before the reply");

    let early = store
        .get(&alice, &early.id)
        .await
        .expect("get")
        .expect("kept");
    assert_eq!(early.status, IntakeItemStatus::HandledByOwner);
    assert_eq!(
        early
            .status_history
            .iter()
            .map(|transition| (transition.status, transition.at))
            .collect::<Vec<_>>(),
        vec![
            (IntakeItemStatus::Received, at(1_700_000_000)),
            (IntakeItemStatus::HandledByOwner, reply_time),
        ]
    );
    for untouched in [&other_chat, &later] {
        let current = store
            .get(&alice, &untouched.id)
            .await
            .expect("get")
            .expect("kept");
        assert_eq!(current.status, IntakeItemStatus::Received);
        assert_eq!(current.status_history.len(), 1);
    }

    // Repeating the same owner activity changes nothing.
    assert_eq!(
        store
            .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", reply_time))
            .await
            .expect("repeat"),
        0
    );
    // Nothing is ever deleted: every item is still listed.
    assert_eq!(
        store
            .list(list_request(&alice, 10))
            .await
            .expect("list")
            .items
            .len(),
        4
    );
}

#[tokio::test]
async fn one_owners_items_are_invisible_to_another_owner() {
    let store = in_memory_intake_store();
    let alice = owner("alice");
    let bob = owner("bob");
    let item = store
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "for alice",
            at(1_700_000_000),
        ))
        .await
        .expect("record")
        .item()
        .clone();

    assert_eq!(store.get(&bob, &item.id).await.expect("get"), None);
    assert!(
        store
            .list(list_request(&bob, 10))
            .await
            .expect("list")
            .items
            .is_empty()
    );
    assert_eq!(
        store
            .mark_chat_handled_by_owner(mark_request(&bob, "chat-1", at(1_700_000_100)))
            .await
            .expect("mark"),
        0
    );
    let still = store
        .get(&alice, &item.id)
        .await
        .expect("get")
        .expect("kept");
    assert_eq!(still.status, IntakeItemStatus::Received);

    // The same event for another owner is a separate record, not a duplicate.
    let bobs = store
        .record(request(
            &bob,
            "event-1",
            "chat-1",
            "for bob",
            at(1_700_000_001),
        ))
        .await
        .expect("record");
    assert!(matches!(bobs, RecordIntakeOutcome::Created(_)));
}

/// A deliberately misconfigured mount: every owner's `/intake` resolves to
/// one shared directory, so one owner's reads reach another owner's rows.
fn shared_mount<F: RootFilesystem>(backend: Arc<F>) -> Arc<ScopedFilesystem<F>> {
    Arc::new(ScopedFilesystem::new(backend, |_scope: &ResourceScope| {
        MountView::new(vec![MountGrant::new(
            MountAlias::new("/intake")?,
            VirtualPath::new("/tenants/tenant-a/users/shared/intake")?,
            MountPermissions::read_write(),
        )])
    }))
}

/// The per-user mount is the isolation boundary; the store's one owner check
/// is its backstop. A row that names another owner is an invariant
/// violation, so every operation fails closed instead of hiding it.
#[tokio::test]
async fn a_mount_that_shares_rows_across_owners_fails_closed() {
    let store = IntakeStore::new(shared_mount(Arc::new(InMemoryBackend::new())));
    let alice = owner("alice");
    let bob = owner("bob");
    let item = store
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "for alice",
            at(1_700_000_000),
        ))
        .await
        .expect("record")
        .item()
        .clone();

    assert!(matches!(
        store.list(list_request(&bob, 10)).await,
        Err(IntakeError::ForeignRecord)
    ));
    assert!(matches!(
        store
            .list(ListIntakeItemsRequest {
                status: Some(IntakeItemStatus::Received),
                ..list_request(&bob, 10)
            })
            .await,
        Err(IntakeError::ForeignRecord)
    ));
    assert!(matches!(
        store.get(&bob, &item.id).await,
        Err(IntakeError::ForeignRecord)
    ));
    assert!(matches!(
        store
            .mark_chat_handled_by_owner(mark_request(&bob, "chat-1", at(1_700_000_100)))
            .await,
        Err(IntakeError::ForeignRecord)
    ));
    assert!(matches!(
        store
            .record(request(
                &bob,
                "event-1",
                "chat-1",
                "for bob",
                at(1_700_000_001)
            ))
            .await,
        Err(IntakeError::ForeignRecord)
    ));

    // Alice's own item is untouched and still readable.
    let still = store
        .get(&alice, &item.id)
        .await
        .expect("get")
        .expect("kept");
    assert_eq!(still.status, IntakeItemStatus::Received);
    assert_eq!(still.text, "for alice");
}

#[tokio::test]
async fn the_store_never_needs_delete_and_the_mount_refuses_it() {
    let backend = Arc::new(InMemoryBackend::new());
    let filesystem = per_user_intake_mount(Arc::clone(&backend));
    let store = IntakeStore::new(Arc::clone(&filesystem));
    let alice = owner("alice");
    let item = store
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "kept",
            at(1_700_000_000),
        ))
        .await
        .expect("record")
        .item()
        .clone();
    store
        .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", at(1_700_000_100)))
        .await
        .expect("mark");

    let mut scope = ResourceScope::system();
    scope.tenant_id = alice.tenant_id.clone();
    scope.user_id = alice.user_id.clone();
    let path = ScopedPath::new(format!("/intake/items/{}.json", item.id)).expect("path");
    let error = filesystem
        .delete(&scope, &path)
        .await
        .expect_err("intake records are retained");
    assert!(matches!(error, FilesystemError::PermissionDenied { .. }));
    assert!(store.get(&alice, &item.id).await.expect("get").is_some());
}

/// The per-user mount shape, but read-only: what a reader that may not write
/// is given.
fn read_only_intake_mount<F: RootFilesystem>(backend: Arc<F>) -> Arc<ScopedFilesystem<F>> {
    Arc::new(ScopedFilesystem::new(backend, |scope: &ResourceScope| {
        MountView::new(vec![MountGrant::new(
            MountAlias::new("/intake")?,
            VirtualPath::new(format!(
                "/tenants/{}/users/{}/intake",
                scope.tenant_id.as_str(),
                scope.user_id.as_str()
            ))?,
            MountPermissions::read_only(),
        )])
    }))
}

/// The read half writes nothing, not even an index declaration: over a mount
/// that refuses every write it lists an owner who never received an item as
/// empty, and reads what the write half stored.
#[tokio::test]
async fn reading_needs_no_write_permission_even_before_the_first_item() {
    let backend = Arc::new(InMemoryBackend::new());
    let reader = IntakeStore::new(read_only_intake_mount(Arc::clone(&backend)));
    let recorder = IntakeStore::new(per_user_intake_mount(Arc::clone(&backend)));
    let alice = owner("alice");

    for status in [None, Some(IntakeItemStatus::Received)] {
        let page = reader
            .list(ListIntakeItemsRequest {
                status,
                ..list_request(&alice, 10)
            })
            .await
            .expect("an owner with no items lists as empty");
        assert!(page.items.is_empty());
        assert_eq!(page.next_cursor, None);
    }

    let item = recorder
        .record(request(
            &alice,
            "event-1",
            "chat-1",
            "hello",
            at(1_700_000_000),
        ))
        .await
        .expect("record")
        .item()
        .clone();
    let page = reader
        .list(list_request(&alice, 10))
        .await
        .expect("list after the first item");
    assert_eq!(page.items, vec![item.clone()]);
    assert_eq!(reader.get(&alice, &item.id).await.expect("get"), Some(item));
    // Another owner still has nothing declared and still reads as empty.
    assert!(
        reader
            .list(list_request(&owner("bob"), 10))
            .await
            .expect("list")
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn record_retries_a_racing_writer_and_fails_closed_when_retries_run_out() {
    let racing = Arc::new(VersionRacingBackend::new(Arc::new(InMemoryBackend::new())));
    let store = IntakeStore::new(per_user_intake_mount(Arc::clone(&racing)));
    let alice = owner("alice");

    racing
        .arm("/tenants/tenant-a/users/alice/intake/items/", 1)
        .await;
    store
        .record(request(
            &alice,
            "event-race",
            "chat-1",
            "raced",
            at(1_700_000_000),
        ))
        .await
        .expect("a transient conflict is retried");
    assert_eq!(racing.injected_count().await, 1);

    racing
        .arm("/tenants/tenant-a/users/alice/intake/items/", u32::MAX)
        .await;
    assert!(matches!(
        store
            .record(request(
                &alice,
                "event-lost",
                "chat-1",
                "lost",
                at(1_700_000_001)
            ))
            .await,
        Err(IntakeError::Backend { .. })
    ));
    let page = store.list(list_request(&alice, 10)).await.expect("list");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].text, "raced");
}

/// Two deliveries of one event racing each other: both read the item path
/// as empty before either writes. The create-if-absent CAS lets exactly one
/// create the item; the other retries, finds it, and reports a duplicate.
#[tokio::test]
async fn two_concurrent_deliveries_of_one_event_store_exactly_one_item() {
    let racing = Arc::new(VersionRacingBackend::new(Arc::new(InMemoryBackend::new())));
    let store = IntakeStore::new(per_user_intake_mount(Arc::clone(&racing)));
    let alice = owner("alice");
    racing
        .hold_reads("/tenants/tenant-a/users/alice/intake/items/", 2)
        .await;
    let delivery = || request(&alice, "event-1", "chat-1", "hello", at(1_700_000_000));
    let (first, second) = tokio::join!(store.record(delivery()), store.record(delivery()));
    let outcomes = [
        first.expect("first delivery"),
        second.expect("second delivery"),
    ];
    let created = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RecordIntakeOutcome::Created(_)))
        .count();
    let duplicates = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RecordIntakeOutcome::Duplicate(_)))
        .count();
    assert_eq!((created, duplicates), (1, 1), "{outcomes:?}");
    assert_eq!(outcomes[0].item(), outcomes[1].item());
    let page = store.list(list_request(&alice, 10)).await.expect("list");
    assert_eq!(page.items.len(), 1);
}

fn owner_scope(owner: &IntakeOwner) -> ResourceScope {
    let mut scope = ResourceScope::system();
    scope.tenant_id = owner.tenant_id.clone();
    scope.user_id = owner.user_id.clone();
    scope
}

/// Overwrites a stored item's body with bytes the store cannot decode,
/// keeping its index entries, as a damaged or future-schema record would.
async fn corrupt<F: RootFilesystem>(
    filesystem: &ScopedFilesystem<F>,
    owner: &IntakeOwner,
    id: &IntakeItemId,
) {
    let scope = owner_scope(owner);
    let path = ScopedPath::new(format!("/intake/items/{id}.json")).expect("item path");
    let mut stored = filesystem
        .get(&scope, &path)
        .await
        .expect("read stored item")
        .expect("item is stored");
    stored.entry.body = br#"{"schema_version":99}"#.to_vec();
    filesystem
        .put(
            &scope,
            &path,
            stored.entry,
            CasExpectation::Version(stored.version),
        )
        .await
        .expect("overwrite stored item");
}

#[tokio::test]
async fn an_undecodable_record_is_skipped_without_breaking_listing_or_the_sweep() {
    let filesystem = per_user_intake_mount(Arc::new(InMemoryBackend::new()));
    let store = IntakeStore::new(Arc::clone(&filesystem));
    let alice = owner("alice");
    let mut ids = Vec::new();
    for (index, text) in ["oldest", "damaged", "newest"].iter().enumerate() {
        ids.push(
            store
                .record(request(
                    &alice,
                    &format!("e-{index}"),
                    "chat-1",
                    text,
                    at(1_700_000_000 + index as i64),
                ))
                .await
                .expect("record")
                .item()
                .clone()
                .id,
        );
    }
    corrupt(&filesystem, &alice, &ids[1]).await;

    let page = store.list(list_request(&alice, 10)).await.expect("list");
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["newest", "oldest"]
    );
    assert_eq!(page.next_cursor, None);

    // One row per page: the damaged row still takes its place in the scan,
    // so its page comes back empty with a cursor past it, and paging ends.
    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..4 {
        let page = store
            .list(ListIntakeItemsRequest {
                cursor: cursor.clone(),
                ..list_request(&alice, 1)
            })
            .await
            .expect("page");
        seen.push(
            page.items
                .iter()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>(),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        seen,
        vec![
            vec!["newest".to_string()],
            Vec::new(),
            vec!["oldest".to_string()]
        ]
    );

    assert_eq!(
        store
            .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", at(1_700_000_100)))
            .await
            .expect("the sweep skips the damaged record"),
        2
    );
    assert!(
        store.get(&alice, &ids[1]).await.is_err(),
        "a direct read of the damaged record still reports it"
    );
}

/// More damaged records than one sweep batch holds: each round resumes after
/// the rows the previous one scanned, so the damaged rows (which stay
/// `received`) cannot stall the sweep or exhaust its bounded rounds.
#[tokio::test]
async fn damaged_records_filling_whole_batches_do_not_stall_the_sweep() {
    let filesystem = per_user_intake_mount(Arc::new(InMemoryBackend::new()));
    let store = IntakeStore::new(Arc::clone(&filesystem));
    let alice = owner("alice");
    for index in 0..150 {
        let item = store
            .record(request(
                &alice,
                &format!("damaged-{index}"),
                "chat-1",
                "damaged",
                at(1_700_000_000 + index),
            ))
            .await
            .expect("record")
            .item()
            .clone();
        corrupt(&filesystem, &alice, &item.id).await;
    }
    store
        .record(request(
            &alice,
            "intact",
            "chat-1",
            "intact",
            at(1_700_001_000),
        ))
        .await
        .expect("record");
    assert_eq!(
        store
            .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", at(1_700_002_000)))
            .await
            .expect("sweep passes the damaged rows"),
        1
    );
}

#[tokio::test]
async fn items_and_their_listing_survive_a_libsql_reopen() {
    let directory = tempfile::tempdir().expect("temporary libSQL directory");
    let database_path = directory.path().join("intake.db");
    let alice = owner("alice");
    let created = {
        let database = Arc::new(
            libsql::Builder::new_local(&database_path)
                .build()
                .await
                .expect("build database"),
        );
        let root = Arc::new(LibSqlRootFilesystem::new(database).expect("build root filesystem"));
        root.run_migrations().await.expect("run migrations");
        let store = IntakeStore::new(per_user_intake_mount(root));
        store
            .record(request(
                &alice,
                "event-1",
                "chat-1",
                "first",
                at(1_700_000_000),
            ))
            .await
            .expect("record");
        store
            .record(request(
                &alice,
                "event-2",
                "chat-1",
                "second",
                at(1_700_000_001),
            ))
            .await
            .expect("record")
            .item()
            .clone()
    };

    let database = Arc::new(
        libsql::Builder::new_local(&database_path)
            .build()
            .await
            .expect("reopen database"),
    );
    let root = Arc::new(LibSqlRootFilesystem::new(database).expect("reopen root filesystem"));
    root.run_migrations().await.expect("rerun migrations");
    let reopened = IntakeStore::new(per_user_intake_mount(root));
    let page = reopened
        .list(list_request(&alice, 10))
        .await
        .expect("list after reopen");
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "first"]
    );
    // An owner with no items has no declared projection on this backend
    // either, and still lists as empty.
    assert!(
        reopened
            .list(list_request(&owner("bob"), 10))
            .await
            .expect("list for an owner with no items")
            .items
            .is_empty()
    );
    assert_eq!(
        reopened.get(&alice, &created.id).await.expect("get"),
        Some(created.clone())
    );
    assert!(matches!(
        reopened
            .record(request(
                &alice,
                "event-2",
                "chat-1",
                "second",
                at(1_700_000_001)
            ))
            .await
            .expect("replay after reopen"),
        RecordIntakeOutcome::Duplicate(_)
    ));
    assert_eq!(
        reopened
            .mark_chat_handled_by_owner(mark_request(&alice, "chat-1", at(1_700_000_100)))
            .await
            .expect("mark after reopen"),
        2
    );
    let id = IntakeItemId::new(created.id.as_str()).expect("id");
    assert_eq!(
        reopened
            .get(&alice, &id)
            .await
            .expect("get")
            .expect("kept")
            .status,
        IntakeItemStatus::HandledByOwner
    );
}

struct VersionRacingBackend {
    inner: Arc<InMemoryBackend>,
    state: Mutex<RacingState>,
}

struct RacingState {
    target_prefix: Option<String>,
    injected: u32,
    remaining: u32,
    /// Reads under this prefix wait for each other, so concurrent writers
    /// all see the same state before any of them writes.
    held_reads: Option<HeldReads>,
}

struct HeldReads {
    prefix: String,
    remaining: usize,
    barrier: Arc<Barrier>,
}

impl VersionRacingBackend {
    fn new(inner: Arc<InMemoryBackend>) -> Self {
        Self {
            inner,
            state: Mutex::new(RacingState {
                target_prefix: None,
                injected: 0,
                remaining: 0,
                held_reads: None,
            }),
        }
    }

    async fn arm(&self, prefix: &str, count: u32) {
        let mut state = self.state.lock().await;
        state.target_prefix = Some(prefix.to_string());
        state.injected = 0;
        state.remaining = count;
    }

    async fn injected_count(&self) -> u32 {
        self.state.lock().await.injected
    }

    /// The next `readers` reads under `prefix` each take their snapshot and
    /// then wait for one another, so all of them read the same state.
    async fn hold_reads(&self, prefix: &str, readers: usize) {
        self.state.lock().await.held_reads = Some(HeldReads {
            prefix: prefix.to_string(),
            remaining: readers,
            barrier: Arc::new(Barrier::new(readers)),
        });
    }
}

#[async_trait]
impl RootFilesystem for VersionRacingBackend {
    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    async fn put(
        &self,
        path: &VirtualPath,
        entry: Entry,
        cas: CasExpectation,
    ) -> Result<RecordVersion, FilesystemError> {
        {
            let mut state = self.state.lock().await;
            if state.remaining > 0
                && state
                    .target_prefix
                    .as_deref()
                    .is_some_and(|prefix| path.as_str().starts_with(prefix))
            {
                state.remaining -= 1;
                state.injected += 1;
                return Err(FilesystemError::VersionMismatch {
                    path: path.clone(),
                    expected: None,
                    found: None,
                });
            }
        }
        self.inner.put(path, entry, cas).await
    }

    async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
        let barrier = {
            let mut state = self.state.lock().await;
            match state.held_reads.as_mut() {
                Some(held) if held.remaining > 0 && path.as_str().starts_with(&held.prefix) => {
                    held.remaining -= 1;
                    Some(Arc::clone(&held.barrier))
                }
                _ => None,
            }
        };
        // Read first, then wait: every held reader has its snapshot before
        // any of them can go on to write.
        let read = self.inner.get(path).await;
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        read
    }

    async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
        self.inner.list_dir(path).await
    }

    async fn query(
        &self,
        path: &VirtualPath,
        filter: &Filter,
        page: Page,
    ) -> Result<Vec<VersionedEntry>, FilesystemError> {
        self.inner.query(path, filter, page).await
    }

    async fn query_ordered(
        &self,
        path: &VirtualPath,
        filter: &Filter,
        page: &OrderedPage,
    ) -> Result<Vec<VersionedEntry>, FilesystemError> {
        self.inner.query_ordered(path, filter, page).await
    }

    async fn ensure_index(
        &self,
        path: &VirtualPath,
        spec: &IndexSpec,
    ) -> Result<(), FilesystemError> {
        self.inner.ensure_index(path, spec).await
    }

    async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
        self.inner.stat(path).await
    }

    async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
        self.inner.delete(path).await
    }
}
