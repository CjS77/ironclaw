//! Operator-configured channel egress endpoints: the binding half.
//!
//! A manifest egress target may name an endpoint id instead of a literal host
//! (`ironclaw_extension_contracts::channel_endpoint`). This module holds what
//! the operator bound each `(extension id, endpoint id)` to — the concrete
//! origin, and whether that origin may sit on a private network — and the one
//! operation the policy layer needs from it: rewriting a checked logical URL
//! onto that origin.
//!
//! Only boot configuration constructs these values; composition maps
//! `[channel_endpoints]` from `config.toml` and hands the set to the host.
//! Nothing tenant- or user-scoped can add or change an entry, so a tenant
//! administrator can never point an extension at an address inside the
//! operator's network.
//!
//! Every loosening is opt-in and off by default: an endpoint is `https`,
//! public-network-only, unless the operator explicitly allows otherwise, and
//! plain `http` additionally requires the private-network opt-in.

use std::collections::BTreeMap;

use ironclaw_extension_contracts::channel::ChannelEgressDescriptor;
use ironclaw_extension_contracts::channel_endpoint::{
    CHANNEL_ENDPOINT_LOGICAL_HOST_SUFFIX, ChannelEgressEndpointId,
};
use ironclaw_host_api::action::{NetworkScheme, NetworkTargetPattern};
use ironclaw_host_api::error::HostApiError;
use ironclaw_host_api::ids::ExtensionId;

/// The host an adapter writes into its request URL for an endpoint:
/// `<id>.channel-endpoint.invalid`.
pub fn logical_host(endpoint: &ChannelEgressEndpointId) -> String {
    format!("{endpoint}.{CHANNEL_ENDPOINT_LOGICAL_HOST_SUFFIX}")
}

/// The host a request to `target` carries: the literal manifest host, or the
/// endpoint's logical host.
pub fn request_host(target: &ChannelEgressDescriptor) -> String {
    match &target.endpoint {
        Some(endpoint) => logical_host(endpoint),
        None => target.host.clone(),
    }
}

/// Whether `host` falls in the reserved logical-endpoint domain. One
/// trailing dot (the DNS root) is ignored, so `x.channel-endpoint.invalid.`
/// cannot pass as a literal host.
fn is_logical_endpoint_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host.strip_suffix('.')
        .unwrap_or(&host)
        .ends_with(CHANNEL_ENDPOINT_LOGICAL_HOST_SUFFIX)
}

/// Read an endpoint id back out of a request host.
///
/// `None` means the host is outside the reserved logical domain, so it is a
/// literal host. `Some(Err)` means it is inside that domain but is not exactly
/// `<valid id>.channel-endpoint.invalid`; the caller must refuse it rather
/// than fall back to treating it as literal. Case-insensitive, like DNS.
pub(crate) fn endpoint_for_logical_host(
    host: &str,
) -> Option<Result<ChannelEgressEndpointId, HostApiError>> {
    if !is_logical_endpoint_host(host) {
        return None;
    }
    // A root-dot form is in the reserved domain but is not the canonical
    // logical host, so it is refused (an empty label never validates).
    let host = host.to_ascii_lowercase();
    let label = host
        .strip_suffix(CHANNEL_ENDPOINT_LOGICAL_HOST_SUFFIX)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .unwrap_or_default();
    Some(ChannelEgressEndpointId::new(label))
}

/// Whether an endpoint's address may resolve into a private network
/// (RFC 1918, loopback, unique-local, carrier-grade NAT). The network layer
/// still refuses its always-blocked ranges either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateNetworkPosture {
    /// Ordinary egress: private and host-local addresses are refused.
    Denied,
    /// The operator opted this one endpoint into private-network reach.
    Allowed,
}

/// Whether an endpoint may be reached over plain `http`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsecureHttpPosture {
    Denied,
    Allowed,
}

/// Why an operator endpoint was refused. Carries no URL text beyond the
/// field the operator must fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperatorChannelEndpointError {
    #[error("endpoint URL does not parse: {reason}")]
    InvalidUrl { reason: String },
    #[error("endpoint URL scheme must be http or https")]
    UnsupportedScheme,
    #[error(
        "an http endpoint requires both allow_insecure_http and allow_private_network to be true"
    )]
    InsecureHttpNotAllowed,
    #[error("endpoint URL must not carry userinfo")]
    Userinfo,
    #[error("endpoint URL must not carry a query or fragment")]
    QueryOrFragment,
    #[error("endpoint URL path must be empty or `/`")]
    Path,
    #[error("endpoint URL must name a literal host (no wildcards, not a logical endpoint host)")]
    Host,
    #[error("endpoint URL must name a host name or IPv4 address; IPv6 literals are not supported")]
    Ipv6Literal,
    #[error("endpoint `{endpoint}` of extension `{extension_id}` is configured twice")]
    Duplicate {
        extension_id: String,
        endpoint: String,
    },
}

/// One operator-bound endpoint: the origin a logical URL is rewritten onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorChannelEndpoint {
    scheme: NetworkScheme,
    /// A host name or IPv4 literal, lowercased as `url::Url::host_str`
    /// renders it — the form the network layer matches targets against.
    host: String,
    /// The effective port: explicit, or the scheme default.
    port: u16,
    explicit_port: bool,
    private_network: PrivateNetworkPosture,
}

impl OperatorChannelEndpoint {
    /// Validate an operator URL into an endpoint. `scheme://host[:port]` with
    /// an empty or `/` path and nothing else.
    pub fn parse(
        raw: &str,
        private_network: PrivateNetworkPosture,
        insecure_http: InsecureHttpPosture,
    ) -> Result<Self, OperatorChannelEndpointError> {
        let url = url::Url::parse(raw.trim()).map_err(|error| {
            OperatorChannelEndpointError::InvalidUrl {
                reason: error.to_string(),
            }
        })?;
        let scheme = match url.scheme() {
            "https" => NetworkScheme::Https,
            "http" => NetworkScheme::Http,
            _ => return Err(OperatorChannelEndpointError::UnsupportedScheme),
        };
        if scheme == NetworkScheme::Http
            && (insecure_http != InsecureHttpPosture::Allowed
                || private_network != PrivateNetworkPosture::Allowed)
        {
            return Err(OperatorChannelEndpointError::InsecureHttpNotAllowed);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OperatorChannelEndpointError::Userinfo);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(OperatorChannelEndpointError::QueryOrFragment);
        }
        if !matches!(url.path(), "" | "/") {
            return Err(OperatorChannelEndpointError::Path);
        }
        // The network layer resolves and matches bare host strings; a
        // bracketed IPv6 literal would fail there on every send, so refuse it
        // here where the operator sees why.
        if matches!(url.host(), Some(url::Host::Ipv6(_))) {
            return Err(OperatorChannelEndpointError::Ipv6Literal);
        }
        let host = url
            .host_str()
            .filter(|host| !host.is_empty() && !host.contains('*'))
            .filter(|host| !is_logical_endpoint_host(host))
            .ok_or(OperatorChannelEndpointError::Host)?
            .to_ascii_lowercase();
        let port = url
            .port_or_known_default()
            .ok_or(OperatorChannelEndpointError::UnsupportedScheme)?;
        Ok(Self {
            scheme,
            host,
            port,
            explicit_port: url.port().is_some(),
            private_network,
        })
    }

    pub fn scheme(&self) -> NetworkScheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn private_network(&self) -> PrivateNetworkPosture {
        self.private_network
    }

    /// The network target transport pins its policy to: this scheme, host,
    /// and port exactly.
    pub fn network_target(&self) -> NetworkTargetPattern {
        NetworkTargetPattern {
            scheme: Some(self.scheme),
            host_pattern: self.host.clone(),
            port: Some(self.port),
        }
    }

    fn origin(&self) -> String {
        let scheme = match self.scheme {
            NetworkScheme::Https => "https",
            NetworkScheme::Http => "http",
        };
        if self.explicit_port {
            format!("{scheme}://{}:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}", self.host)
        }
    }

    /// Move a policy-checked logical URL onto this endpoint's origin.
    ///
    /// The path and query are kept byte for byte as the adapter wrote them —
    /// a declared path placeholder such as `{token}` must survive for
    /// host-side injection — and only the `https://<logical host>` prefix is
    /// replaced. `None` when the URL does not start with exactly that prefix
    /// or the result does not parse back to this origin; the caller refuses
    /// the request.
    pub(crate) fn rewrite(&self, request_url: &str, logical_host: &str) -> Option<String> {
        const LOGICAL_SCHEME: &str = "https://";
        let scheme = request_url.get(..LOGICAL_SCHEME.len())?;
        if !scheme.eq_ignore_ascii_case(LOGICAL_SCHEME) {
            return None;
        }
        let after_scheme = request_url.get(LOGICAL_SCHEME.len()..)?;
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let (authority, rest) = after_scheme.split_at(authority_end);
        if !authority.eq_ignore_ascii_case(logical_host) || rest.starts_with('#') {
            return None;
        }
        let rewritten = if rest.starts_with('/') {
            format!("{}{rest}", self.origin())
        } else {
            format!("{}/{rest}", self.origin())
        };
        let parsed = url::Url::parse(&rewritten).ok()?;
        let lands_on_origin = parsed.host_str() == Some(self.host.as_str())
            && parsed.port_or_known_default() == Some(self.port)
            && parsed.username().is_empty()
            && parsed.password().is_none();
        lands_on_origin.then_some(rewritten)
    }
}

/// Every endpoint the operator configured, keyed by extension and endpoint.
/// Empty by default: no extension can reach an endpoint nobody bound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorChannelEndpoints {
    entries: BTreeMap<(ExtensionId, ChannelEgressEndpointId), OperatorChannelEndpoint>,
}

impl OperatorChannelEndpoints {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind one endpoint. A second binding for the same pair is refused
    /// rather than silently replacing the first.
    pub fn insert(
        &mut self,
        extension_id: ExtensionId,
        endpoint_id: ChannelEgressEndpointId,
        endpoint: OperatorChannelEndpoint,
    ) -> Result<(), OperatorChannelEndpointError> {
        match self.entries.entry((extension_id, endpoint_id)) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                let (extension_id, endpoint_id) = entry.key();
                Err(OperatorChannelEndpointError::Duplicate {
                    extension_id: extension_id.as_str().to_string(),
                    endpoint: endpoint_id.as_str().to_string(),
                })
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(endpoint);
                Ok(())
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The binding for one extension's endpoint, if the operator made one.
    /// An extension id the boundary vocabulary rejects has no bindings.
    pub fn get(
        &self,
        extension_id: &str,
        endpoint_id: &ChannelEgressEndpointId,
    ) -> Option<&OperatorChannelEndpoint> {
        let extension_id = ExtensionId::new(extension_id).ok()?; // silent-ok: an invalid id cannot have been bound, so it has no endpoint
        self.entries.get(&(extension_id, endpoint_id.clone()))
    }

    /// The first endpoint `declared` names that the operator did not bind
    /// for `extension_id` — what activation refuses on.
    pub fn first_unconfigured<'a>(
        &self,
        extension_id: &str,
        declared: &'a [ChannelEgressDescriptor],
    ) -> Option<&'a ChannelEgressEndpointId> {
        declared
            .iter()
            .filter_map(|target| target.endpoint.as_ref())
            .find(|endpoint| self.get(extension_id, endpoint).is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn https(raw: &str) -> Result<OperatorChannelEndpoint, OperatorChannelEndpointError> {
        OperatorChannelEndpoint::parse(
            raw,
            PrivateNetworkPosture::Denied,
            InsecureHttpPosture::Denied,
        )
    }

    #[test]
    fn operator_urls_are_bare_origins() {
        let endpoint = https("https://relay.internal:8443/").expect("bare origin with a port");
        assert_eq!(endpoint.host(), "relay.internal");
        assert_eq!(endpoint.port(), 8443);
        assert_eq!(
            endpoint.network_target(),
            NetworkTargetPattern {
                scheme: Some(NetworkScheme::Https),
                host_pattern: "relay.internal".to_string(),
                port: Some(8443),
            }
        );
        assert_eq!(https("https://relay.internal").unwrap().port(), 443);

        for (raw, expected) in [
            (
                "ftp://relay.internal",
                OperatorChannelEndpointError::UnsupportedScheme,
            ),
            (
                "https://user:pw@relay.internal",
                OperatorChannelEndpointError::Userinfo,
            ),
            (
                "https://relay.internal/?x=1",
                OperatorChannelEndpointError::QueryOrFragment,
            ),
            (
                "https://relay.internal/#frag",
                OperatorChannelEndpointError::QueryOrFragment,
            ),
            (
                "https://relay.internal/v1",
                OperatorChannelEndpointError::Path,
            ),
            ("https://*.internal", OperatorChannelEndpointError::Host),
            (
                "https://relay.channel-endpoint.invalid",
                OperatorChannelEndpointError::Host,
            ),
            (
                "http://relay.internal:8080",
                OperatorChannelEndpointError::InsecureHttpNotAllowed,
            ),
            (
                "https://[fd00::1]:8443",
                OperatorChannelEndpointError::Ipv6Literal,
            ),
            (
                "https://relay.channel-endpoint.invalid.",
                OperatorChannelEndpointError::Host,
            ),
        ] {
            assert_eq!(https(raw).unwrap_err(), expected, "{raw}");
        }
        assert!(matches!(
            https("not a url"),
            Err(OperatorChannelEndpointError::InvalidUrl { .. })
        ));
    }

    #[test]
    fn logical_hosts_round_trip_and_malformed_ones_are_refused() {
        let id = ChannelEgressEndpointId::new("relay").unwrap();
        assert_eq!(logical_host(&id), "relay.channel-endpoint.invalid");
        assert_eq!(
            endpoint_for_logical_host("RELAY.Channel-Endpoint.Invalid")
                .unwrap()
                .unwrap(),
            id
        );
        assert!(endpoint_for_logical_host("api.vendor.example").is_none());
        for malformed in [
            "channel-endpoint.invalid",
            "a.b.channel-endpoint.invalid",
            "evilchannel-endpoint.invalid",
            "bad_id.channel-endpoint.invalid",
            "relay.channel-endpoint.invalid.",
        ] {
            assert!(
                matches!(endpoint_for_logical_host(malformed), Some(Err(_))),
                "{malformed}"
            );
            assert!(is_logical_endpoint_host(malformed));
        }
    }

    #[test]
    fn plain_http_needs_both_opt_ins() {
        let raw = "http://relay:8080";
        for (private, insecure) in [
            (PrivateNetworkPosture::Allowed, InsecureHttpPosture::Denied),
            (PrivateNetworkPosture::Denied, InsecureHttpPosture::Allowed),
        ] {
            assert_eq!(
                OperatorChannelEndpoint::parse(raw, private, insecure).unwrap_err(),
                OperatorChannelEndpointError::InsecureHttpNotAllowed
            );
        }
        let endpoint = OperatorChannelEndpoint::parse(
            raw,
            PrivateNetworkPosture::Allowed,
            InsecureHttpPosture::Allowed,
        )
        .expect("both opt-ins admit http");
        assert_eq!(endpoint.scheme(), NetworkScheme::Http);
        assert_eq!(endpoint.private_network(), PrivateNetworkPosture::Allowed);
    }

    #[test]
    fn rewrite_replaces_only_the_logical_origin() {
        let endpoint = OperatorChannelEndpoint::parse(
            "http://127.0.0.1:8080",
            PrivateNetworkPosture::Allowed,
            InsecureHttpPosture::Allowed,
        )
        .unwrap();
        let host = "relay.channel-endpoint.invalid";
        assert_eq!(
            endpoint
                .rewrite("https://relay.channel-endpoint.invalid/v1/send?x=1", host)
                .as_deref(),
            Some("http://127.0.0.1:8080/v1/send?x=1")
        );
        assert_eq!(
            endpoint
                .rewrite("HTTPS://Relay.Channel-Endpoint.Invalid/bot{token}/x", host)
                .as_deref(),
            Some("http://127.0.0.1:8080/bot{token}/x"),
            "placeholders survive for host-side injection"
        );
        assert_eq!(
            endpoint
                .rewrite("https://relay.channel-endpoint.invalid", host)
                .as_deref(),
            Some("http://127.0.0.1:8080/")
        );
        for refused in [
            "http://relay.channel-endpoint.invalid/v1",
            "https://other.channel-endpoint.invalid/v1",
            "https://relay.channel-endpoint.invalid:443/v1",
            "https://relay.channel-endpoint.invalid#x",
            // Inputs the `url` crate normalizes but the string-level prefix
            // check does not: each must be refused, never retargeted.
            "https://relay.channel-endpoint.invalid\\v1",
            "https://re\tlay.channel-endpoint.invalid/v1",
            "https://relay%2Echannel-endpoint.invalid/v1",
            "https://relay.channel-endpoint.invalid./v1",
            "https:relay.channel-endpoint.invalid/v1",
            " https://relay.channel-endpoint.invalid/v1",
        ] {
            assert_eq!(endpoint.rewrite(refused, host), None, "{refused}");
        }
    }

    #[test]
    fn bindings_are_unique_and_scoped_to_their_extension() {
        let relay = ChannelEgressEndpointId::new("relay").unwrap();
        let mut endpoints = OperatorChannelEndpoints::new();
        endpoints
            .insert(
                ExtensionId::new("acme-inbox").unwrap(),
                relay.clone(),
                https("https://relay.internal").unwrap(),
            )
            .unwrap();
        assert!(matches!(
            endpoints.insert(
                ExtensionId::new("acme-inbox").unwrap(),
                relay.clone(),
                https("https://other.internal").unwrap(),
            ),
            Err(OperatorChannelEndpointError::Duplicate { .. })
        ));
        assert!(endpoints.get("acme-inbox", &relay).is_some());
        assert!(
            endpoints.get("acme-other", &relay).is_none(),
            "another extension's binding is not shared"
        );
        assert_eq!(endpoints.len(), 1);
    }
}
