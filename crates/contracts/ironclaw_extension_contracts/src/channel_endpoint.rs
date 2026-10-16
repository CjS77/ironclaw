//! Operator-configured egress endpoints: vocabulary only.
//!
//! A `[[channel.egress]]` target may name an endpoint id instead of a literal
//! host when only the deployment knows the service's address (a sidecar on a
//! private network). The operator binds the id in `config.toml`; adapters
//! address it at `https://<endpoint_id>.channel-endpoint.invalid/<path>`
//! (`.invalid` is reserved by RFC 2606, so it can never resolve). Building and
//! parsing those addresses, the bindings, and the origin rewrite live in
//! `ironclaw_extension_host::channel_endpoints`.

use ironclaw_host_api::error::HostApiError;
use serde::{Deserialize, Serialize};

/// The reserved DNS suffix every logical endpoint host ends with.
pub const CHANNEL_ENDPOINT_LOGICAL_HOST_SUFFIX: &str = "channel-endpoint.invalid";

/// Longest endpoint id: one DNS label, so the logical host stays valid.
const MAX_ENDPOINT_ID_BYTES: usize = 63;

/// The id a manifest gives an operator-configured egress endpoint.
///
/// One DNS label — lowercase ASCII letters, digits and `-`, starting and
/// ending with a letter or digit, at most 63 bytes — because it becomes the
/// first label of the logical host. Scoped to its extension: the operator
/// binds `(extension id, endpoint id)`, so two extensions may both name a
/// `relay` endpoint without sharing it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ChannelEgressEndpointId(String);

impl ChannelEgressEndpointId {
    fn validate(value: &str) -> Result<(), HostApiError> {
        let invalid = |reason: &str| HostApiError::InvalidId {
            kind: "channel_egress_endpoint",
            value: value.to_string(),
            reason: reason.to_string(),
        };
        if value.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if value.len() > MAX_ENDPOINT_ID_BYTES {
            return Err(invalid("must be at most 63 bytes"));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid(
                "must be one DNS label: lowercase ASCII letters, digits, and '-'",
            ));
        }
        if value.starts_with('-') || value.ends_with('-') {
            return Err(invalid("must start and end with a letter or digit"));
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, HostApiError> {
        let value = raw.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ChannelEgressEndpointId {
    type Error = HostApiError;

    fn try_from(value: String) -> Result<Self, HostApiError> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for ChannelEgressEndpointId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChannelEgressEndpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ChannelEgressEndpointId> for String {
    fn from(id: ChannelEgressEndpointId) -> Self {
        id.0
    }
}
// Deliberately no `From<String>` and no `Deref<Target = str>`: either would
// let an unvalidated string pass as an endpoint id.

// The endpoint half of `ChannelEgressDescriptor`, kept beside the endpoint
// vocabulary so the descriptor module does not grow.
impl crate::channel::ChannelEgressDescriptor {
    /// The target as diagnostics name it: the host, or `endpoint:<id>`.
    pub fn label(&self) -> String {
        match &self.endpoint {
            Some(endpoint) => format!("endpoint:{endpoint}"),
            None => self.host.clone(),
        }
    }
}
