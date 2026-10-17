use thiserror::Error;

/// Failure of an intake store operation.
///
/// `Display` never carries the backend or serialization cause: callers map it
/// onto model-visible and webhook-visible outcomes. The cause stays on the
/// `reason` field for server-side logs.
#[derive(Debug, Error)]
pub enum IntakeError {
    #[error("intake store backend unavailable")]
    Backend { reason: String },
    #[error("intake record serialization failed")]
    Serialization { reason: String },
    #[error("intake request rejected: {reason}")]
    InvalidRequest { reason: &'static str },
    /// Two different inbound events derived the same item id. The existing
    /// record is never overwritten.
    #[error("intake item id conflicts with a different inbound event")]
    Conflict,
    /// A stored record names a different owner than the one whose per-user
    /// mount it was read from: the mount is misconfigured. The store fails
    /// closed rather than hide or disclose the row.
    #[error("intake record belongs to a different owner")]
    ForeignRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_and_serialization_errors_retain_causes_without_exposing_them() {
        let backend = IntakeError::Backend {
            reason: "backend detail".to_string(),
        };
        assert_eq!(backend.to_string(), "intake store backend unavailable");
        assert!(matches!(
            backend,
            IntakeError::Backend { ref reason } if reason == "backend detail"
        ));

        let serialization = IntakeError::Serialization {
            reason: "serialization detail".to_string(),
        };
        assert_eq!(
            serialization.to_string(),
            "intake record serialization failed"
        );
    }
}
