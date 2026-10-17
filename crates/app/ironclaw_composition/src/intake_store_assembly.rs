use std::sync::Arc;

use ironclaw_filesystem::CompositeRootFilesystem;
use ironclaw_intake::IntakeStore;

/// The one intake store composition builds. Held concretely: it has a single
/// production implementation, and each consumer receives only its half by
/// unsized coercion at the use site — the intake channel sink the
/// `IntakeRecorder`, the read tools the `IntakeReader`.
pub(crate) type ComposedIntakeStore = IntakeStore<CompositeRootFilesystem>;

/// Builds the intake item store over the composition-owned filesystem. Items
/// land under each owner's per-user `/intake` mount, which grants no delete.
pub(crate) fn build_intake_store(
    filesystem: Arc<CompositeRootFilesystem>,
) -> Arc<ComposedIntakeStore> {
    #[allow(clippy::disallowed_methods)]
    let store = IntakeStore::new(crate::wrap_scoped(filesystem));
    Arc::new(store)
}
