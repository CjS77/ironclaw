//! Channel restricted egress: the policy half of `RestrictedEgress` for
//! channel adapters (OUT-8).
//!
//! The resolved `[[channel.egress]]` declarations are the sole authority:
//! scheme/host/method allowlisting, host-owned-header rejection, and
//! credential-handle screening all happen here, **before** any transport
//! activity. What leaves this module is an [`ApprovedChannelEgress`] — a
//! policy-approved plan carrying the declared injection target — executed by
//! the composition-injected [`ChannelEgressTransport`] (which resolves secret
//! material and drives the host runtime egress with its private-IP/redirect
//! denial and response caps). Adapters never see secret bytes; a buggy or
//! malicious adapter cannot name an undeclared host, method, or credential.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_extension_contracts::channel_endpoint::ChannelEgressEndpointId;
use ironclaw_extension_contracts::tool_adapter::{
    RestrictedEgress, RestrictedEgressError, RestrictedEgressRequest, RestrictedEgressResponse,
};
use ironclaw_host_api::{
    action::{NetworkMethod, NetworkScheme, NetworkTargetPattern},
    http::RuntimeCredentialTarget,
    ids::SecretHandle,
};

use crate::channel_endpoints::{OperatorChannelEndpoints, PrivateNetworkPosture};
use crate::lifecycle::EgressFactory;

/// Default response-body cap for channel vendor calls without an explicit
/// manifest bound.
pub const CHANNEL_EGRESS_RESPONSE_BODY_LIMIT_BYTES: u64 = 256 * 1024;
/// Default per-call timeout for channel vendor calls.
pub const CHANNEL_EGRESS_TIMEOUT_MS: u64 = 10_000;

/// Headers the host owns; an adapter supplying one is rejected before any
/// network activity (`authorization` belongs to declared credential
/// injection; the rest are transport-owned or hop-by-hop).
const HOST_OWNED_HEADERS: &[&str] = &[
    "authorization",
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
    "te",
    "trailer",
];

/// One egress target's declared policy, resolved from the manifest.
#[derive(Debug, Clone)]
pub struct DeclaredChannelEgress {
    pub scheme: NetworkScheme,
    /// The literal host; empty for an endpoint target.
    pub host: String,
    /// The operator-configured endpoint this target names instead of a host.
    pub endpoint: Option<ChannelEgressEndpointId>,
    pub methods: Vec<NetworkMethod>,
    pub credential_handle: Option<SecretHandle>,
    pub injection: Option<RuntimeCredentialTarget>,
    /// Declared body-credential bindings: each maps a handle to the RFC 6901
    /// JSON pointer where the host inserts its resolved value. A request
    /// naming an undeclared handle is rejected before any transport activity.
    pub body_credentials:
        Vec<ironclaw_extension_contracts::channel::ChannelBodyCredentialDescriptor>,
    pub paths: Vec<String>,
    pub path_prefixes: Vec<String>,
    pub request_body_limit_bytes: Option<u64>,
    pub response_body_limit_bytes: Option<u64>,
}

/// The credential part of an approved plan: the declared handle plus the
/// declared injection target (defaulting to `Authorization: Bearer`).
#[derive(Debug, Clone)]
pub struct ApprovedChannelCredential {
    pub handle: SecretHandle,
    pub target: RuntimeCredentialTarget,
}

/// A policy-approved vendor call. Everything here has passed the declared
/// allowlist; the transport must still enforce private-IP/redirect denial and
/// the response cap at the network boundary.
#[derive(Debug, Clone)]
pub struct ApprovedChannelEgress {
    pub extension_id: String,
    pub installation_id: String,
    pub method: NetworkMethod,
    /// Full URL; when the injection target is a path placeholder the
    /// `{placeholder}` is still present — the transport substitutes secret
    /// material host-side.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The single network target the transport must pin its policy to.
    pub target: PinnedEgressTarget,
    pub credential: Option<ApprovedChannelCredential>,
    /// Declared body credentials this call opted into: handle plus its
    /// manifest-declared `BodyJsonPointer` target. Resolution and insertion
    /// are the transport's job, host-side.
    pub body_credentials: Vec<ApprovedChannelCredential>,
    /// Manifest-declared request-body cap. The policy layer checks the
    /// adapter-supplied body before approval; the host runtime must check the
    /// final body again after credential injection.
    pub request_body_limit: Option<u64>,
    pub response_body_limit: u64,
    pub timeout_ms: u64,
}

/// The one network target an approved call may reach, and whether that target
/// may resolve into a private network.
///
/// A literal manifest host pins `https`, any port the URL did not name, and
/// private ranges denied. An operator endpoint pins the operator's exact
/// scheme and port, and carries the private-network opt-in only when the
/// operator gave it for that endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedEgressTarget {
    pub pattern: NetworkTargetPattern,
    pub private_network: PrivateNetworkPosture,
}

impl PinnedEgressTarget {
    fn literal(host: String) -> Self {
        Self {
            pattern: NetworkTargetPattern {
                scheme: Some(NetworkScheme::Https),
                host_pattern: host,
                port: None,
            },
            private_network: PrivateNetworkPosture::Denied,
        }
    }
}

/// Executes approved plans. Implemented by composition over the host runtime
/// egress (secret-material resolution + injection + SSRF-safe transport).
#[async_trait]
pub trait ChannelEgressTransport: Send + Sync {
    async fn execute(
        &self,
        approved: ApprovedChannelEgress,
    ) -> Result<RestrictedEgressResponse, RestrictedEgressError>;
}

/// Per-extension channel egress: declared policy + injected transport.
pub struct PolicyEnforcedChannelEgress {
    extension_id: String,
    installation_id: String,
    declared: Vec<DeclaredChannelEgress>,
    transport: Arc<dyn ChannelEgressTransport>,
    endpoints: Arc<OperatorChannelEndpoints>,
}

impl PolicyEnforcedChannelEgress {
    pub fn new(
        extension_id: impl Into<String>,
        installation_id: impl Into<String>,
        declared: Vec<DeclaredChannelEgress>,
        transport: Arc<dyn ChannelEgressTransport>,
        endpoints: Arc<OperatorChannelEndpoints>,
    ) -> Self {
        Self {
            extension_id: extension_id.into(),
            installation_id: installation_id.into(),
            declared,
            transport,
            endpoints,
        }
    }

    fn approve(
        &self,
        request: RestrictedEgressRequest,
    ) -> Result<ApprovedChannelEgress, RestrictedEgressError> {
        let url = url::Url::parse(&request.url).map_err(|_| RestrictedEgressError::PolicyDenied)?;
        if !channel_url_shape_is_safe(&url) {
            return Err(RestrictedEgressError::PolicyDenied);
        }
        let scheme = match url.scheme() {
            "https" => NetworkScheme::Https,
            _ => return Err(RestrictedEgressError::PolicyDenied),
        };
        let host = url
            .host_str()
            .ok_or(RestrictedEgressError::PolicyDenied)?
            .to_ascii_lowercase();
        let request_path = url.path();
        // A host in the reserved logical domain addresses an endpoint target
        // and nothing else; a malformed one is refused, never read as literal.
        let endpoint = match crate::channel_endpoints::endpoint_for_logical_host(&host) {
            None => None,
            Some(Ok(endpoint)) => Some(endpoint),
            Some(Err(_)) => return Err(RestrictedEgressError::UndeclaredHost { host }),
        };
        let same_host: Vec<&DeclaredChannelEgress> = self
            .declared
            .iter()
            .filter(|target| match (&endpoint, &target.endpoint) {
                (Some(requested), Some(declared)) => requested == declared,
                (None, None) => target.scheme == scheme && target.host.eq_ignore_ascii_case(&host),
                _ => false,
            })
            .collect();
        if same_host.is_empty() {
            return Err(RestrictedEgressError::UndeclaredHost { host });
        }
        let declared = same_host.iter().copied().find(|target| {
            target.methods.contains(&request.method) && target.matches_path(request_path)
        });
        let Some(declared) = declared else {
            if same_host
                .iter()
                .any(|target| target.methods.contains(&request.method))
            {
                return Err(RestrictedEgressError::PolicyDenied);
            }
            return Err(RestrictedEgressError::UndeclaredMethod);
        };
        for (name, _) in &request.headers {
            if HOST_OWNED_HEADERS
                .iter()
                .any(|owned| name.eq_ignore_ascii_case(owned))
            {
                return Err(RestrictedEgressError::HostOwnedHeader { name: name.clone() });
            }
        }
        let credential = match (&request.credential, &declared.credential_handle) {
            (None, _) => None,
            (Some(handle), Some(declared_handle)) if handle == declared_handle => {
                Some(ApprovedChannelCredential {
                    handle: handle.clone(),
                    target: declared.injection.clone().unwrap_or_else(|| {
                        RuntimeCredentialTarget::Header {
                            name: "authorization".to_string(),
                            prefix: Some("Bearer ".to_string()),
                        }
                    }),
                })
            }
            (Some(handle), _) => {
                return Err(RestrictedEgressError::UndeclaredCredential {
                    handle: handle.as_str().to_string(),
                });
            }
        };
        let mut body_credentials = Vec::new();
        for handle in &request.body_credentials {
            let declared_binding = declared
                .body_credentials
                .iter()
                .find(|binding| &binding.handle == handle)
                .ok_or_else(|| RestrictedEgressError::UndeclaredCredential {
                    handle: handle.as_str().to_string(),
                })?;
            // A duplicate opt-in would double-insert at the same pointer;
            // reject it as the adapter bug it is.
            if body_credentials
                .iter()
                .any(|approved: &ApprovedChannelCredential| &approved.handle == handle)
            {
                return Err(RestrictedEgressError::UndeclaredCredential {
                    handle: handle.as_str().to_string(),
                });
            }
            body_credentials.push(ApprovedChannelCredential {
                handle: handle.clone(),
                target: RuntimeCredentialTarget::BodyJsonPointer {
                    pointer: declared_binding.pointer.clone(),
                    post_injection_body_limit_bytes: None,
                },
            });
        }
        let body = request.body.unwrap_or_default();
        if declared
            .request_body_limit_bytes
            .is_some_and(|limit| body.len() as u64 > limit)
        {
            return Err(RestrictedEgressError::PolicyDenied);
        }
        // Every check above ran on the URL the adapter wrote. Only now does an
        // endpoint target move onto the operator's origin — and only if the
        // operator bound one for this extension.
        let (url, target) = match &declared.endpoint {
            None => (request.url, PinnedEgressTarget::literal(host)),
            Some(endpoint_id) => {
                let operator = self
                    .endpoints
                    .get(&self.extension_id, endpoint_id)
                    .ok_or(RestrictedEgressError::PolicyDenied)?;
                let rewritten = operator
                    .rewrite(&request.url, &host)
                    .ok_or(RestrictedEgressError::PolicyDenied)?;
                let target = PinnedEgressTarget {
                    pattern: operator.network_target(),
                    private_network: operator.private_network(),
                };
                (rewritten, target)
            }
        };
        Ok(ApprovedChannelEgress {
            extension_id: self.extension_id.clone(),
            installation_id: self.installation_id.clone(),
            method: request.method,
            url,
            headers: request.headers,
            body,
            target,
            credential,
            body_credentials,
            request_body_limit: declared.request_body_limit_bytes,
            response_body_limit: declared
                .response_body_limit_bytes
                .unwrap_or(CHANNEL_EGRESS_RESPONSE_BODY_LIMIT_BYTES)
                .min(ironclaw_extension_contracts::channel::MAX_CHANNEL_EGRESS_TRANSFER_BYTES),
            timeout_ms: CHANNEL_EGRESS_TIMEOUT_MS,
        })
    }
}

/// URL structure authorized by a channel egress declaration.
///
/// Declarations grant a scheme, host, method, and path/path-prefix. Query
/// parameters are request data on that already-authorized endpoint; fragments,
/// userinfo, and explicit ports can change request authority or interpretation
/// and remain forbidden. Percent-decoded path segments must also remain one
/// segment and cannot become traversal instructions at a downstream parser.
fn channel_url_shape_is_safe(url: &url::Url) -> bool {
    url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && channel_path_shape_is_safe(url.path())
}

pub(crate) fn channel_path_shape_is_safe(path: &str) -> bool {
    !path.contains(['?', '#'])
        && path.split('/').all(|segment| {
            let decoded = ironclaw_network::percent_decode_url_component_lossy(segment);
            decoded != "."
                && decoded != ".."
                && !decoded.as_bytes().contains(&b'/')
                && !decoded.as_bytes().contains(&b'\\')
        })
}

#[async_trait]
impl RestrictedEgress for PolicyEnforcedChannelEgress {
    async fn send(
        &self,
        request: RestrictedEgressRequest,
    ) -> Result<RestrictedEgressResponse, RestrictedEgressError> {
        let approved = self.approve(request)?;
        let response_body_limit = approved.response_body_limit;
        let response = self.transport.execute(approved).await?;
        if response.body.len() as u64 > response_body_limit {
            // Defensive double-check; the transport enforces the cap at the
            // network boundary.
            return Err(RestrictedEgressError::ResponseTooLarge);
        }
        Ok(response)
    }
}

impl DeclaredChannelEgress {
    pub(crate) fn matches_path(&self, request_path: &str) -> bool {
        if self.paths.is_empty() && self.path_prefixes.is_empty() {
            return true;
        }
        self.paths
            .iter()
            .any(|path| normalized_declared_path(path) == request_path)
            || self.path_prefixes.iter().any(|prefix| {
                // Segment-boundary match, not a raw byte prefix. Descriptor
                // validation already requires a trailing `/`, but enforcing it
                // here too means a prefix that reached policy by any other
                // route still cannot authorize a sibling path that merely
                // shares its bytes (`/file/botX` vs `/file/botXEvil/…`).
                let declared = normalized_declared_path(prefix);
                let declared = declared.strip_suffix('/').unwrap_or(&declared);
                request_path
                    .strip_prefix(declared)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
    }

    /// Match one manifest recipe path against this manifest declaration.
    /// Both sides still contain credential placeholders, so normalize both;
    /// [`Self::matches_path`] instead receives an already URL-normalized wire
    /// request path from `url::Url`.
    pub(crate) fn matches_recipe_path(&self, recipe_path: &str) -> bool {
        let normalized = normalized_declared_path(recipe_path);
        self.matches_path(&normalized)
    }

    /// Lift one resolved `[[channel.egress]]` declaration into policy form.
    pub fn from_descriptor(
        descriptor: &ironclaw_extension_contracts::channel::ChannelEgressDescriptor,
    ) -> Self {
        Self {
            scheme: descriptor.scheme,
            host: descriptor.host.clone(),
            endpoint: descriptor.endpoint.clone(),
            methods: descriptor.methods.clone(),
            credential_handle: descriptor.credential_handle.clone(),
            injection: descriptor.injection.clone(),
            body_credentials: descriptor.body_credentials.clone(),
            paths: descriptor.paths.clone(),
            path_prefixes: descriptor.path_prefixes.clone(),
            request_body_limit_bytes: descriptor.request_body_limit_bytes,
            response_body_limit_bytes: descriptor.response_body_limit_bytes,
        }
    }
}

fn normalized_declared_path(path: &str) -> String {
    url::Url::parse(&format!("https://manifest.invalid{path}"))
        .map(|url| url.path().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// The production [`EgressFactory`]: builds a policy-enforced egress from the
/// declaration the lifecycle passes (staged or active) over one injected
/// transport and the operator's endpoint bindings. An extension with no
/// declared egress gets a policy that rejects every host — fail-closed, never
/// a panic.
///
/// The ingress router and the delivery resolver build their per-call egress
/// through [`Self::policy_for_channel`] too, so every channel egress in the
/// process reads the same endpoint bindings.
pub struct TransportBackedEgressFactory {
    transport: Arc<dyn ChannelEgressTransport>,
    endpoints: Arc<OperatorChannelEndpoints>,
}

impl TransportBackedEgressFactory {
    pub fn new(
        transport: Arc<dyn ChannelEgressTransport>,
        endpoints: Arc<OperatorChannelEndpoints>,
    ) -> Self {
        Self {
            transport,
            endpoints,
        }
    }

    /// The concrete policy egress for one channel's declared targets.
    pub fn policy_for_channel(
        &self,
        extension_id: &str,
        installation_id: &str,
        declared: &[ironclaw_extension_contracts::channel::ChannelEgressDescriptor],
    ) -> PolicyEnforcedChannelEgress {
        PolicyEnforcedChannelEgress::new(
            extension_id,
            installation_id,
            declared
                .iter()
                .map(DeclaredChannelEgress::from_descriptor)
                .collect(),
            Arc::clone(&self.transport),
            Arc::clone(&self.endpoints),
        )
    }
}

impl EgressFactory for TransportBackedEgressFactory {
    fn egress_for_channel(
        &self,
        extension_id: &str,
        installation_id: &str,
        declared: &[ironclaw_extension_contracts::channel::ChannelEgressDescriptor],
    ) -> Arc<dyn RestrictedEgress> {
        Arc::new(self.policy_for_channel(extension_id, installation_id, declared))
    }

    fn unconfigured_endpoint(
        &self,
        extension_id: &str,
        declared: &[ironclaw_extension_contracts::channel::ChannelEgressDescriptor],
    ) -> Option<ChannelEgressEndpointId> {
        self.endpoints
            .first_unconfigured(extension_id, declared)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_endpoints::{InsecureHttpPosture, OperatorChannelEndpoint};
    use ironclaw_host_api::ids::ExtensionId;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingTransport {
        approved: Mutex<Vec<ApprovedChannelEgress>>,
    }

    #[async_trait]
    impl ChannelEgressTransport for RecordingTransport {
        async fn execute(
            &self,
            approved: ApprovedChannelEgress,
        ) -> Result<RestrictedEgressResponse, RestrictedEgressError> {
            self.approved.lock().unwrap().push(approved);
            Ok(RestrictedEgressResponse {
                retry_after: None,
                status: 200,
                body: b"{}".to_vec(),
            })
        }
    }

    fn declared_vendor() -> Vec<DeclaredChannelEgress> {
        vec![DeclaredChannelEgress {
            scheme: NetworkScheme::Https,
            host: "vendor.example".to_string(),
            endpoint: None,
            methods: vec![NetworkMethod::Post],
            credential_handle: Some(SecretHandle::new("vendor_bot_token").unwrap()),
            injection: None,
            body_credentials: Vec::new(),
            paths: Vec::new(),
            path_prefixes: Vec::new(),
            request_body_limit_bytes: None,
            response_body_limit_bytes: None,
        }]
    }

    fn egress_over(
        declared: Vec<DeclaredChannelEgress>,
    ) -> (PolicyEnforcedChannelEgress, Arc<RecordingTransport>) {
        let transport = Arc::new(RecordingTransport::default());
        (
            PolicyEnforcedChannelEgress::new(
                "vendorx",
                "inst-1",
                declared,
                transport.clone() as Arc<dyn ChannelEgressTransport>,
                Arc::new(OperatorChannelEndpoints::new()),
            ),
            transport,
        )
    }

    fn post(url: &str) -> RestrictedEgressRequest {
        RestrictedEgressRequest {
            method: NetworkMethod::Post,
            url: url.to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(b"{}".to_vec()),
            credential: Some(SecretHandle::new("vendor_bot_token").unwrap()),
            body_credentials: Vec::new(),
        }
    }

    #[tokio::test]
    async fn undeclared_host_is_rejected_before_any_transport_activity() {
        let (egress, transport) = egress_over(declared_vendor());
        let error = egress
            .send(post("https://evil.example/api/x"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredHost { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_https_and_undeclared_method_are_rejected_before_transport() {
        let (egress, transport) = egress_over(declared_vendor());
        let error = egress
            .send(post("http://vendor.example/api/x"))
            .await
            .unwrap_err();
        assert!(matches!(error, RestrictedEgressError::PolicyDenied));

        let mut get = post("https://vendor.example/api/x");
        get.method = NetworkMethod::Get;
        let error = egress.send(get).await.unwrap_err();
        assert!(matches!(error, RestrictedEgressError::UndeclaredMethod));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn adapter_supplied_authorization_header_is_rejected() {
        let (egress, transport) = egress_over(declared_vendor());
        let mut request = post("https://vendor.example/api/x");
        request
            .headers
            .push(("Authorization".to_string(), "Bearer stolen".to_string()));
        let error = egress.send(request).await.unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::HostOwnedHeader { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn undeclared_credential_handle_is_rejected() {
        let (egress, transport) = egress_over(declared_vendor());
        let mut request = post("https://vendor.example/api/x");
        request.credential = Some(SecretHandle::new("some_other_secret").unwrap());
        let error = egress.send(request).await.unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredCredential { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn approved_plan_carries_default_bearer_injection_and_pinned_host() {
        let (egress, transport) = egress_over(declared_vendor());
        egress
            .send(post("https://vendor.example/api/chat.postMessage"))
            .await
            .unwrap();
        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].target.pattern.host_pattern, "vendor.example");
        assert_eq!(
            approved[0].target.private_network,
            PrivateNetworkPosture::Denied
        );
        assert_eq!(approved[0].extension_id, "vendorx");
        let credential = approved[0].credential.as_ref().unwrap();
        assert!(matches!(
            &credential.target,
            RuntimeCredentialTarget::Header { name, prefix }
                if name == "authorization" && prefix.as_deref() == Some("Bearer ")
        ));
    }

    #[tokio::test]
    async fn approved_plan_carries_declared_path_placeholder_injection() {
        let mut declared = declared_vendor();
        declared[0].injection = Some(RuntimeCredentialTarget::PathPlaceholder {
            placeholder: "token".to_string(),
        });
        let (egress, transport) = egress_over(declared);
        egress
            .send(post("https://vendor.example/bot{token}/sendMessage"))
            .await
            .unwrap();
        let approved = transport.approved.lock().unwrap();
        let credential = approved[0].credential.as_ref().unwrap();
        assert!(matches!(
            &credential.target,
            RuntimeCredentialTarget::PathPlaceholder { placeholder } if placeholder == "token"
        ));
        assert!(
            approved[0].url.contains("{token}"),
            "placeholder substitution is the transport's job, host-side"
        );
    }

    #[tokio::test]
    async fn declared_body_credential_is_approved_with_its_declared_pointer() {
        let mut declared = declared_vendor();
        declared[0].request_body_limit_bytes = Some(256);
        declared[0].body_credentials = vec![
            ironclaw_extension_contracts::channel::ChannelBodyCredentialDescriptor {
                handle: SecretHandle::new("vendor_webhook_secret").unwrap(),
                pointer: "/secret_token".to_string(),
            },
        ];
        let (egress, transport) = egress_over(declared);
        let mut request = post("https://vendor.example/api/setWebhook");
        request.body_credentials = vec![SecretHandle::new("vendor_webhook_secret").unwrap()];
        egress.send(request).await.unwrap();
        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved[0].body_credentials.len(), 1);
        assert_eq!(
            approved[0].body_credentials[0].handle.as_str(),
            "vendor_webhook_secret"
        );
        assert_eq!(approved[0].request_body_limit, Some(256));
        assert!(
            matches!(
                &approved[0].body_credentials[0].target,
                RuntimeCredentialTarget::BodyJsonPointer { pointer, .. }
                    if pointer == "/secret_token"
            ),
            "the pointer comes from the DECLARATION, never from the adapter"
        );
    }

    #[tokio::test]
    async fn undeclared_body_credential_handle_is_rejected_before_transport() {
        // No body_credentials declared at all: any opt-in is rejected.
        let (egress, transport) = egress_over(declared_vendor());
        let mut request = post("https://vendor.example/api/setWebhook");
        request.body_credentials = vec![SecretHandle::new("vendor_webhook_secret").unwrap()];
        let error = egress.send(request).await.unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredCredential { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());

        // Declared for a DIFFERENT handle: still rejected.
        let mut declared = declared_vendor();
        declared[0].body_credentials = vec![
            ironclaw_extension_contracts::channel::ChannelBodyCredentialDescriptor {
                handle: SecretHandle::new("vendor_webhook_secret").unwrap(),
                pointer: "/secret_token".to_string(),
            },
        ];
        let (egress, transport) = egress_over(declared);
        let mut request = post("https://vendor.example/api/setWebhook");
        request.body_credentials = vec![SecretHandle::new("some_other_secret").unwrap()];
        let error = egress.send(request).await.unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredCredential { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_body_credential_opt_in_is_rejected_before_transport() {
        let mut declared = declared_vendor();
        declared[0].body_credentials = vec![
            ironclaw_extension_contracts::channel::ChannelBodyCredentialDescriptor {
                handle: SecretHandle::new("vendor_webhook_secret").unwrap(),
                pointer: "/secret_token".to_string(),
            },
        ];
        let (egress, transport) = egress_over(declared);
        let mut request = post("https://vendor.example/api/setWebhook");
        request.body_credentials = vec![
            SecretHandle::new("vendor_webhook_secret").unwrap(),
            SecretHandle::new("vendor_webhook_secret").unwrap(),
        ];
        let error = egress.send(request).await.unwrap_err();
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredCredential { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversized_transport_response_is_rejected() {
        struct HugeTransport;
        #[async_trait]
        impl ChannelEgressTransport for HugeTransport {
            async fn execute(
                &self,
                _approved: ApprovedChannelEgress,
            ) -> Result<RestrictedEgressResponse, RestrictedEgressError> {
                Ok(RestrictedEgressResponse {
                    retry_after: None,
                    status: 200,
                    body: vec![0u8; (CHANNEL_EGRESS_RESPONSE_BODY_LIMIT_BYTES + 1) as usize],
                })
            }
        }
        let egress = PolicyEnforcedChannelEgress::new(
            "vendorx",
            "inst-1",
            declared_vendor(),
            Arc::new(HugeTransport),
            Arc::new(OperatorChannelEndpoints::new()),
        );
        let error = egress
            .send(post("https://vendor.example/api/x"))
            .await
            .unwrap_err();
        assert!(matches!(error, RestrictedEgressError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn declared_path_and_body_bounds_are_enforced_before_transport() {
        let mut declared = declared_vendor();
        declared[0].paths = vec!["/api/exact".to_string()];
        declared[0].path_prefixes = vec!["/files/".to_string()];
        declared[0].request_body_limit_bytes = Some(2);
        let (egress, transport) = egress_over(declared);

        egress
            .send(post("https://vendor.example/api/exact"))
            .await
            .expect("exact path is declared");
        egress
            .send(post("https://vendor.example/files/report.pdf"))
            .await
            .expect("prefix path is declared");

        let error = egress
            .send(post("https://vendor.example/api/exact-suffix"))
            .await
            .expect_err("exact declaration must not be prefix-matched");
        assert!(matches!(error, RestrictedEgressError::PolicyDenied));

        let mut oversized = post("https://vendor.example/api/exact");
        oversized.body = Some(b"123".to_vec());
        let error = egress
            .send(oversized)
            .await
            .expect_err("oversized body must be denied");
        assert!(matches!(error, RestrictedEgressError::PolicyDenied));
        assert_eq!(transport.approved.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn query_data_is_allowed_but_authority_changing_url_components_are_rejected() {
        let mut declared = declared_vendor();
        declared[0].path_prefixes = vec!["/files/".to_string()];
        let (egress, transport) = egress_over(declared);

        egress
            .send(post(
                "https://vendor.example/files/report.pdf?download=1&file=F123",
            ))
            .await
            .expect("query data on an authorized endpoint is allowed");

        for url in [
            "https://vendor.example/files/report.pdf#fragment",
            "https://user@vendor.example/files/report.pdf",
            "https://vendor.example:8443/files/report.pdf",
            "https://vendor.example/files/%2e%2e%2fsecret",
            "https://vendor.example/files/%2e%2e%5csecret",
        ] {
            let error = egress
                .send(post(url))
                .await
                .expect_err("undeclared URL structure must fail closed");
            assert!(
                matches!(error, RestrictedEgressError::PolicyDenied),
                "unexpected error for {url}: {error:?}"
            );
        }

        assert_eq!(transport.approved.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_host_targets_are_selected_by_method_and_path_with_declared_response_cap() {
        let mut post_target = declared_vendor().remove(0);
        post_target.paths = vec!["/api/getFile".to_string()];
        post_target.response_body_limit_bytes = Some(64 * 1024);
        let mut download_target = post_target.clone();
        download_target.methods = vec![NetworkMethod::Get];
        download_target.paths.clear();
        download_target.path_prefixes = vec!["/file/".to_string()];
        download_target.response_body_limit_bytes = Some(5 * 1024 * 1024);
        let (egress, transport) = egress_over(vec![post_target, download_target]);

        let mut download = post("https://vendor.example/file/report.pdf");
        download.method = NetworkMethod::Get;
        download.body = None;
        egress
            .send(download)
            .await
            .expect("download target matches");

        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].method, NetworkMethod::Get);
        assert_eq!(approved[0].response_body_limit, 5 * 1024 * 1024);
    }

    #[tokio::test]
    async fn placeholder_paths_and_zero_request_bound_match_the_shipped_declaration() {
        let mut exact = declared_vendor().remove(0);
        exact.paths = vec!["/bot{token}/getFile".to_string()];
        let mut download = exact.clone();
        download.methods = vec![NetworkMethod::Get];
        download.paths.clear();
        download.path_prefixes = vec!["/file/bot{token}/".to_string()];
        download.request_body_limit_bytes = Some(0);
        let (egress, transport) = egress_over(vec![exact, download]);

        egress
            .send(post("https://vendor.example/bot{token}/getFile"))
            .await
            .expect("tokenized exact path is declared");

        let mut get = post("https://vendor.example/file/bot{token}/photos/x.png");
        get.method = NetworkMethod::Get;
        get.body = None;
        egress
            .send(get)
            .await
            .expect("tokenized prefix with an empty GET body is declared");

        assert_eq!(transport.approved.lock().unwrap().len(), 2);
    }

    /// Prefix matching is a segment match, not a byte-prefix match. A declared
    /// `/file/bot123/` must not authorize `/file/bot123Evil/...`: that sibling
    /// shares the declared bytes but is a different path the manifest author
    /// never allowed on this pinned host and credential.
    #[tokio::test]
    async fn path_prefix_does_not_authorize_a_sibling_sharing_its_bytes() {
        let mut target = declared_vendor().remove(0);
        target.methods = vec![NetworkMethod::Get];
        target.paths.clear();
        target.path_prefixes = vec!["/file/bot123/".to_string()];
        let (egress, transport) = egress_over(vec![target]);

        let mut sibling = post("https://vendor.example/file/bot123Evil/secrets.txt");
        sibling.method = NetworkMethod::Get;
        sibling.body = None;
        let error = egress
            .send(sibling)
            .await
            .expect_err("a sibling path sharing the prefix bytes must be denied");
        assert!(matches!(error, RestrictedEgressError::PolicyDenied));

        let mut allowed = post("https://vendor.example/file/bot123/report.pdf");
        allowed.method = NetworkMethod::Get;
        allowed.body = None;
        egress
            .send(allowed)
            .await
            .expect("a genuine sub-path of the declared prefix is allowed");

        assert_eq!(transport.approved.lock().unwrap().len(), 1);
    }

    // ---- operator-configured endpoint targets --------------------------------

    const LOGICAL: &str = "https://relay.channel-endpoint.invalid";

    fn relay_id() -> ChannelEgressEndpointId {
        ChannelEgressEndpointId::new("relay").unwrap()
    }

    fn declared_relay() -> DeclaredChannelEgress {
        let mut target = declared_vendor().remove(0);
        target.host = String::new();
        target.endpoint = Some(relay_id());
        target.paths = vec!["/v1/messages".to_string()];
        target
    }

    fn operator_endpoints(raw: &str, private: PrivateNetworkPosture) -> OperatorChannelEndpoints {
        let insecure = if raw.starts_with("http://") {
            InsecureHttpPosture::Allowed
        } else {
            InsecureHttpPosture::Denied
        };
        let mut endpoints = OperatorChannelEndpoints::new();
        endpoints
            .insert(
                ExtensionId::new("vendorx").unwrap(),
                relay_id(),
                OperatorChannelEndpoint::parse(raw, private, insecure).unwrap(),
            )
            .unwrap();
        endpoints
    }

    fn egress_with_endpoints(
        declared: Vec<DeclaredChannelEgress>,
        endpoints: OperatorChannelEndpoints,
    ) -> (PolicyEnforcedChannelEgress, Arc<RecordingTransport>) {
        let transport = Arc::new(RecordingTransport::default());
        (
            PolicyEnforcedChannelEgress::new(
                "vendorx",
                "inst-1",
                declared,
                transport.clone() as Arc<dyn ChannelEgressTransport>,
                Arc::new(endpoints),
            ),
            transport,
        )
    }

    #[tokio::test]
    async fn a_logical_endpoint_url_is_rewritten_onto_the_operator_origin() {
        let (egress, transport) = egress_with_endpoints(
            vec![declared_relay()],
            operator_endpoints("http://127.0.0.1:8080", PrivateNetworkPosture::Allowed),
        );
        egress
            .send(post(&format!("{LOGICAL}/v1/messages?trace=1")))
            .await
            .expect("a declared, configured endpoint target is approved");
        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].url, "http://127.0.0.1:8080/v1/messages?trace=1");
        assert_eq!(
            approved[0].target,
            PinnedEgressTarget {
                pattern: NetworkTargetPattern {
                    scheme: Some(NetworkScheme::Http),
                    host_pattern: "127.0.0.1".to_string(),
                    port: Some(8080),
                },
                private_network: PrivateNetworkPosture::Allowed,
            }
        );
        assert!(
            approved[0].credential.is_some(),
            "the declared credential still rides the endpoint target"
        );
    }

    #[tokio::test]
    async fn an_https_endpoint_without_the_opt_in_keeps_private_ranges_denied() {
        let (egress, transport) = egress_with_endpoints(
            vec![declared_relay()],
            operator_endpoints("https://relay.internal:8443", PrivateNetworkPosture::Denied),
        );
        egress
            .send(post(&format!("{LOGICAL}/v1/messages")))
            .await
            .expect("approved; the transport enforces the private-range denial");
        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved[0].url, "https://relay.internal:8443/v1/messages");
        assert_eq!(
            approved[0].target.private_network,
            PrivateNetworkPosture::Denied
        );
    }

    #[tokio::test]
    async fn undeclared_unconfigured_and_malformed_endpoints_are_denied_before_transport() {
        // Declared, but the operator bound nothing for it.
        let (egress, transport) =
            egress_with_endpoints(vec![declared_relay()], OperatorChannelEndpoints::new());
        let error = egress
            .send(post(&format!("{LOGICAL}/v1/messages")))
            .await
            .expect_err("an unconfigured endpoint is denied");
        assert!(
            matches!(error, RestrictedEgressError::PolicyDenied),
            "{error:?}"
        );
        assert!(transport.approved.lock().unwrap().is_empty());

        let (egress, transport) = egress_with_endpoints(
            vec![declared_relay()],
            operator_endpoints("https://relay.internal", PrivateNetworkPosture::Denied),
        );
        for url in [
            // An endpoint the manifest never declared.
            "https://other.channel-endpoint.invalid/v1/messages",
            // Malformed labels in the reserved domain never fall back to literal.
            "https://channel-endpoint.invalid/v1/messages",
            "https://a.relay.channel-endpoint.invalid/v1/messages",
        ] {
            let error = egress.send(post(url)).await.expect_err(url);
            assert!(
                matches!(error, RestrictedEgressError::UndeclaredHost { .. }),
                "{url}: {error:?}"
            );
        }
        // The operator's real address is not itself a declared host.
        let error = egress
            .send(post("https://relay.internal/v1/messages"))
            .await
            .expect_err("the resolved origin is not addressable directly");
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredHost { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn adapter_supplied_authority_on_a_logical_url_is_denied() {
        let (egress, transport) = egress_with_endpoints(
            vec![declared_relay()],
            operator_endpoints("http://127.0.0.1:8080", PrivateNetworkPosture::Allowed),
        );
        for url in [
            "https://relay.channel-endpoint.invalid:8080/v1/messages",
            "https://user@relay.channel-endpoint.invalid/v1/messages",
            "https://relay.channel-endpoint.invalid/v1/messages#frag",
            "https://relay.channel-endpoint.invalid/v1/%2e%2e%2fmessages",
            // The logical scheme is always https; the operator decides the real one.
            "http://relay.channel-endpoint.invalid/v1/messages",
        ] {
            let error = egress.send(post(url)).await.expect_err(url);
            assert!(
                matches!(error, RestrictedEgressError::PolicyDenied),
                "{url}: {error:?}"
            );
        }
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn method_and_path_rules_apply_to_endpoint_targets() {
        let (egress, transport) = egress_with_endpoints(
            vec![declared_relay()],
            operator_endpoints("http://127.0.0.1:8080", PrivateNetworkPosture::Allowed),
        );
        let error = egress
            .send(post(&format!("{LOGICAL}/v1/admin")))
            .await
            .expect_err("an undeclared path is denied");
        assert!(matches!(error, RestrictedEgressError::PolicyDenied));
        let mut get = post(&format!("{LOGICAL}/v1/messages"));
        get.method = NetworkMethod::Get;
        let error = egress.send(get).await.expect_err("an undeclared method");
        assert!(matches!(error, RestrictedEgressError::UndeclaredMethod));
        let mut stolen = post(&format!("{LOGICAL}/v1/messages"));
        stolen.credential = Some(SecretHandle::new("some_other_secret").unwrap());
        let error = egress
            .send(stolen)
            .await
            .expect_err("undeclared credential");
        assert!(matches!(
            error,
            RestrictedEgressError::UndeclaredCredential { .. }
        ));
        assert!(transport.approved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_literal_host_target_never_gets_the_private_network_exemption() {
        // The operator bound `relay` to a private address with the opt-in, and
        // a literal target names that very host: the literal target still
        // pins https, no port, and private ranges denied.
        let mut literal = declared_vendor().remove(0);
        literal.host = "127.0.0.1".to_string();
        let (egress, transport) = egress_with_endpoints(
            vec![literal, declared_relay()],
            operator_endpoints("http://127.0.0.1:8080", PrivateNetworkPosture::Allowed),
        );
        egress
            .send(post("https://127.0.0.1/api/x"))
            .await
            .expect("literal targets are approved as before");
        let approved = transport.approved.lock().unwrap();
        assert_eq!(approved[0].url, "https://127.0.0.1/api/x");
        assert_eq!(
            approved[0].target,
            PinnedEgressTarget {
                pattern: NetworkTargetPattern {
                    scheme: Some(NetworkScheme::Https),
                    host_pattern: "127.0.0.1".to_string(),
                    port: None,
                },
                private_network: PrivateNetworkPosture::Denied,
            }
        );
    }

    #[test]
    fn the_factory_reports_the_first_unconfigured_endpoint_for_activation() {
        let descriptor: ironclaw_extension_contracts::channel::ChannelEgressDescriptor =
            serde_json::from_value(serde_json::json!({
                "endpoint": "relay",
                "methods": ["post"],
            }))
            .unwrap();
        let declared = vec![descriptor];
        let unconfigured = TransportBackedEgressFactory::new(
            Arc::new(RecordingTransport::default()),
            Arc::new(OperatorChannelEndpoints::new()),
        );
        assert_eq!(
            unconfigured.unconfigured_endpoint("vendorx", &declared),
            Some(relay_id())
        );
        let configured = TransportBackedEgressFactory::new(
            Arc::new(RecordingTransport::default()),
            Arc::new(operator_endpoints(
                "https://relay.internal",
                PrivateNetworkPosture::Denied,
            )),
        );
        assert_eq!(configured.unconfigured_endpoint("vendorx", &declared), None);
        assert_eq!(
            configured.unconfigured_endpoint("another-extension", &declared),
            Some(relay_id()),
            "a binding belongs to one extension"
        );
    }
}
