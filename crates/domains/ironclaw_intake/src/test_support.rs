//! Test fixtures for consumers of the intake store (the `test-support`
//! feature; never enabled by a shipped artifact).

use std::sync::Arc;

use ironclaw_filesystem::{InMemoryBackend, RootFilesystem, ScopedFilesystem};
use ironclaw_host_api::{
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
    resource::ResourceScope,
};

use crate::IntakeStore;

/// A scoped filesystem over `backend` shaped like production's per-user
/// mount: each owner's `/intake` resolves under that owner's own
/// `/tenants/<tenant>/users/<user>/intake` subtree, and the grant carries no
/// delete permission.
pub fn per_user_intake_mount<F: RootFilesystem>(backend: Arc<F>) -> Arc<ScopedFilesystem<F>> {
    Arc::new(ScopedFilesystem::new(backend, |scope: &ResourceScope| {
        MountView::new(vec![MountGrant::new(
            MountAlias::new("/intake")?,
            VirtualPath::new(format!(
                "/tenants/{}/users/{}/intake",
                scope.tenant_id.as_str(),
                scope.user_id.as_str()
            ))?,
            MountPermissions::read_write(),
        )])
    }))
}

/// An intake store over a fresh in-memory backend and the per-user mount.
pub fn in_memory_intake_store() -> Arc<IntakeStore<InMemoryBackend>> {
    Arc::new(IntakeStore::new(per_user_intake_mount(Arc::new(
        InMemoryBackend::new(),
    ))))
}
