//! Host egress for IronHub calls made outside capability dispatch.
//!
//! `ironclaw ironhub …` and hub-delivered installs call the IronHub service
//! directly, so the capability obligation pipeline never stages a network
//! policy for their invocation and the host egress would refuse every request.
//! This adapter stages the fixed IronHub policy ([`artifact_network_policy`])
//! for exactly as long as an IronHub request is in flight — the same policy a
//! `builtin.ironhub_*` grant stages — so both paths are held to one whitelist.
//! It stages that fixed policy, never the policy a request carries.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::{
    http::{
        RuntimeHttpEgress, RuntimeHttpEgressError, RuntimeHttpEgressRequest,
        RuntimeHttpEgressResponse,
    },
    ids::CapabilityId,
    resource::ResourceScope,
};
use ironclaw_host_runtime::{ProductAuthProviderRuntimePorts, ProductAuthRuntimeHandoffGuard};

use super::artifact_hosts::{artifact_network_policy, check_download_url};
use super::capabilities::{
    IRONHUB_INFO_CAPABILITY_ID, IRONHUB_INSTALL_CAPABILITY_ID, IRONHUB_SEARCH_CAPABILITY_ID,
};

pub struct IronhubDirectEgress {
    ports: ProductAuthProviderRuntimePorts,
    staged: Arc<Mutex<Vec<StagedPolicy>>>,
}

/// One staged policy, shared by every in-flight request with the same scope
/// and capability. An install fetches bundled files concurrently under one
/// scope; revoking per request would pull the policy out from under a sibling
/// that has staged but not yet read it.
struct StagedPolicy {
    scope: ResourceScope,
    capability_id: CapabilityId,
    in_flight: usize,
    _guard: ProductAuthRuntimeHandoffGuard,
}

/// Releases one in-flight reference when the request returns or its future is
/// dropped; the last release drops the handoff guard, which revokes the policy.
struct Lease {
    staged: Arc<Mutex<Vec<StagedPolicy>>>,
    scope: ResourceScope,
    capability_id: CapabilityId,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut staged = self
            .staged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(index) = staged.iter().position(|entry| {
            entry.scope == self.scope && entry.capability_id == self.capability_id
        }) {
            staged[index].in_flight -= 1;
            if staged[index].in_flight == 0 {
                staged.swap_remove(index);
            }
        }
    }
}

impl IronhubDirectEgress {
    pub fn new(ports: ProductAuthProviderRuntimePorts) -> Self {
        Self {
            ports,
            staged: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn lease(&self, scope: &ResourceScope, capability_id: &CapabilityId) -> Lease {
        let mut staged = self
            .staged
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match staged
            .iter_mut()
            .find(|entry| &entry.scope == scope && &entry.capability_id == capability_id)
        {
            Some(entry) => entry.in_flight += 1,
            None => {
                // Guard first, so a failed stage can never leave authority behind.
                let guard = self
                    .ports
                    .staged_handoff_guard(scope.clone(), capability_id.clone());
                self.ports.stage_network_policy_once(
                    scope,
                    capability_id,
                    artifact_network_policy(),
                );
                staged.push(StagedPolicy {
                    scope: scope.clone(),
                    capability_id: capability_id.clone(),
                    in_flight: 1,
                    _guard: guard,
                });
            }
        }
        Lease {
            staged: Arc::clone(&self.staged),
            scope: scope.clone(),
            capability_id: capability_id.clone(),
        }
    }
}

impl std::fmt::Debug for IronhubDirectEgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("IronhubDirectEgress").finish()
    }
}

#[async_trait]
impl RuntimeHttpEgress for IronhubDirectEgress {
    async fn execute(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        admit(&request)?;
        let _lease = self.lease(&request.scope, &request.capability_id);
        self.ports.runtime_http_egress().execute(request).await
    }

    async fn execute_credential_exchange(
        &self,
        _request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        Err(refused(
            "IronHub egress never performs credential exchanges",
        ))
    }
}

/// Only IronHub's own requests may borrow the IronHub policy: one of its three
/// capability ids, no injected credentials, and a whitelisted starting URL.
fn admit(request: &RuntimeHttpEgressRequest) -> Result<(), RuntimeHttpEgressError> {
    if ![
        IRONHUB_SEARCH_CAPABILITY_ID,
        IRONHUB_INFO_CAPABILITY_ID,
        IRONHUB_INSTALL_CAPABILITY_ID,
    ]
    .contains(&request.capability_id.as_str())
    {
        return Err(refused("IronHub egress serves IronHub capabilities only"));
    }
    if !request.credential_injections.is_empty() {
        return Err(refused("IronHub egress never injects credentials"));
    }
    let url =
        url::Url::parse(&request.url).map_err(|_| refused("IronHub egress URL is invalid"))?;
    check_download_url(&url)
        .map_err(|_| refused("IronHub egress URL is not on the download whitelist"))
}

fn refused(reason: &str) -> RuntimeHttpEgressError {
    RuntimeHttpEgressError::Request {
        reason: reason.to_string(),
        request_bytes: 0,
        response_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ironclaw_host_api::{action::NetworkMethod, runtime::RuntimeKind};
    use ironclaw_network::{
        NetworkHttpEgress, NetworkHttpError, NetworkHttpRequest, NetworkHttpResponse, NetworkUsage,
    };

    use super::*;

    const WHITELISTED_URL: &str =
        "https://github.com/CjS77/naomi-addons/releases/download/v1/manifest.json";

    #[derive(Default)]
    struct CountingNetwork {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl NetworkHttpEgress for CountingNetwork {
        async fn execute(
            &self,
            _request: NetworkHttpRequest,
        ) -> Result<NetworkHttpResponse, NetworkHttpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(NetworkHttpResponse {
                status: 200,
                headers: Vec::new(),
                body: b"ok".to_vec(),
                usage: NetworkUsage::default(),
            })
        }
    }

    async fn fixture(
        owner: &str,
    ) -> (
        ProductAuthProviderRuntimePorts,
        Arc<CountingNetwork>,
        ResourceScope,
    ) {
        let network = Arc::new(CountingNetwork::default());
        let services = crate::lifecycle_test_support::build_lifecycle_test_services(
            owner,
            Some(Arc::clone(&network) as Arc<dyn NetworkHttpEgress>),
            false,
        )
        .await;
        let ports = services.runtime_ports.expect("runtime ports with egress");
        let scope = crate::lifecycle_test_support::webui_gate_resource_scope_for_owner(owner);
        (ports, network, scope)
    }

    fn request(scope: &ResourceScope, capability_id: &str, url: &str) -> RuntimeHttpEgressRequest {
        RuntimeHttpEgressRequest {
            runtime: RuntimeKind::FirstParty,
            scope: scope.clone(),
            capability_id: CapabilityId::new(capability_id).expect("capability id"),
            method: NetworkMethod::Get,
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            network_policy: artifact_network_policy(),
            credential_injections: Vec::new(),
            response_body_limit: Some(1024),
            save_body_to: None,
            timeout_ms: Some(1_000),
        }
    }

    fn policy_missing(result: &Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError>) -> bool {
        matches!(
            result,
            Err(RuntimeHttpEgressError::Network { reason, .. }) if reason == "network_policy_missing"
        )
    }

    /// The policy exists only while an IronHub request holds it: before and
    /// after, the raw host egress has nothing staged for that scope. Two
    /// overlapping requests share one staging, and the first to finish must not
    /// revoke it from under the second.
    #[tokio::test]
    async fn stages_the_ironhub_policy_only_while_a_request_is_in_flight() {
        let (ports, network, scope) = fixture("ironhub-direct-egress-lease-owner").await;
        let raw = ports.runtime_http_egress();
        let egress = IronhubDirectEgress::new(ports);
        let install = || request(&scope, IRONHUB_INSTALL_CAPABILITY_ID, WHITELISTED_URL);

        assert!(policy_missing(&raw.execute(install()).await));
        assert_eq!(network.calls.load(Ordering::SeqCst), 0);

        egress
            .execute(install())
            .await
            .expect("IronHub request reaches the network");
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);
        assert!(
            policy_missing(&raw.execute(install()).await),
            "the policy must be revoked once the request returns"
        );

        let capability_id = CapabilityId::new(IRONHUB_INSTALL_CAPABILITY_ID).expect("id");
        let first = egress.lease(&scope, &capability_id);
        let second = egress.lease(&scope, &capability_id);
        drop(first);
        raw.execute(install())
            .await
            .expect("an overlapping request keeps the policy staged");
        drop(second);
        assert!(policy_missing(&raw.execute(install()).await));
    }

    #[tokio::test]
    async fn refuses_requests_that_are_not_ironhub_downloads() {
        let (ports, network, scope) = fixture("ironhub-direct-egress-admit-owner").await;
        let egress = IronhubDirectEgress::new(ports);

        for (capability_id, url) in [
            ("builtin.http", WHITELISTED_URL),
            (
                IRONHUB_SEARCH_CAPABILITY_ID,
                "https://github.com/attacker/repo/releases/download/v1/manifest.json",
            ),
            (
                IRONHUB_INFO_CAPABILITY_ID,
                "https://raw.githubusercontent.com/CjS77/naomi-addons/main/manifest.json",
            ),
        ] {
            let result = egress.execute(request(&scope, capability_id, url)).await;
            assert!(
                matches!(result, Err(RuntimeHttpEgressError::Request { .. })),
                "{capability_id} {url}: {result:?}"
            );
        }
        assert!(matches!(
            egress
                .execute_credential_exchange(request(
                    &scope,
                    IRONHUB_INSTALL_CAPABILITY_ID,
                    WHITELISTED_URL
                ))
                .await,
            Err(RuntimeHttpEgressError::Request { .. })
        ));
        assert_eq!(network.calls.load(Ordering::SeqCst), 0);
    }
}
