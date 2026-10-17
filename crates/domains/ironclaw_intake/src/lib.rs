//! Durable intake items: verified inbound messages that an intake-mode
//! channel stores for the account owner instead of starting an agent turn.
//!
//! Each item is one record at a path derived from its event identity, so a
//! replayed webhook resolves to the record that already exists. Items are
//! never deleted; their status moves forward and every change is kept in the
//! item's history.

mod error;
mod store;
#[cfg(feature = "test-support")]
pub mod test_support;
mod types;

pub use error::IntakeError;
pub use store::IntakeStore;
pub use types::{
    INTAKE_ATTACHMENTS_MAX, INTAKE_PAGE_LIMIT_MAX, INTAKE_TEXT_MAX_BYTES, IntakeAttachmentMetadata,
    IntakeItem, IntakeItemId, IntakeItemStatus, IntakeOwner, IntakePage, IntakeReader,
    IntakeRecorder, IntakeStatusTransition, ListIntakeItemsRequest, MarkChatHandledRequest,
    RecordIntakeItemRequest, RecordIntakeOutcome,
};
