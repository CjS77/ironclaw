//! IronHub downloads through the production egress stack: the hardcoded host
//! whitelist every redirect hop is re-checked against, on both the model
//! capability path and the direct (CLI / hub-delivered) path.
//!
//! These drive the real `HostHttpEgressService` + `PolicyNetworkHttpEgress`
//! pair (only the socket transport and DNS are scripted), so every redirect hop
//! is re-authorized exactly as in a deployed binary.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::resolution::{Resolution, ToolVerdict};
use ironclaw_loop_contracts::{InMemoryLoopHostMilestoneSink, RegisterProviderToolCallRequest};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpError, NetworkHttpResponse, NetworkHttpTransport,
    NetworkResolver, NetworkTransportRequest, NetworkUsage, PolicyNetworkHttpEgress,
};
use ironclaw_threads::InMemorySessionThreadService;

use super::{
    UnavailableModelGateway, UserId, VisibleCapabilityRequest, capability_wiring,
    enable_global_auto_approve_for_run, ensure_thread_for_run, invocation_for_candidate,
    provider_tool_call_with_name, run_context,
};

const GITHUB_MANIFEST_URL: &str =
    "https://github.com/CjS77/naomi-addons/releases/download/v1/manifest.json";

/// Replays one scripted response per call and records every URL it was asked
/// to fetch, so a test can see which redirect hops the policy let through.
#[derive(Clone)]
struct ScriptedTransport {
    responses: Arc<Mutex<Vec<NetworkHttpResponse>>>,
    urls: Arc<Mutex<Vec<String>>>,
}

impl ScriptedTransport {
    fn new(mut responses: Vec<NetworkHttpResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Arc::new(Mutex::new(responses)),
            urls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().expect("urls lock").clone()
    }
}

#[async_trait::async_trait]
impl NetworkHttpTransport for ScriptedTransport {
    async fn execute(
        &self,
        request: NetworkTransportRequest,
    ) -> Result<NetworkHttpResponse, NetworkHttpError> {
        self.urls
            .lock()
            .expect("urls lock")
            .push(request.url.clone());
        Ok(self
            .responses
            .lock()
            .expect("responses lock")
            .pop()
            .expect("transport called more times than scripted"))
    }
}

#[derive(Clone)]
struct PublicResolver;

impl NetworkResolver for PublicResolver {
    fn resolve_ips(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, NetworkHttpError> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(140, 82, 112, 3))])
    }
}

fn redirect_to(location: &str) -> NetworkHttpResponse {
    NetworkHttpResponse {
        status: 302,
        headers: vec![("location".to_string(), location.to_string())],
        body: Vec::new(),
        usage: NetworkUsage::default(),
    }
}

fn unsigned_body() -> NetworkHttpResponse {
    NetworkHttpResponse {
        status: 200,
        headers: Vec::new(),
        body: b"not a signed envelope".to_vec(),
        usage: NetworkUsage::default(),
    }
}

fn manifest_url(url: &str) -> ironclaw_extension_manager::ironhub::IronhubManifestUrl {
    ironclaw_extension_manager::ironhub::validated_manifest_url(url)
        .expect("manifest URL is accepted")
}

fn policy_enforcing_network(transport: &ScriptedTransport) -> Arc<dyn NetworkHttpEgress> {
    Arc::new(PolicyNetworkHttpEgress::new_with_resolver(
        transport.clone(),
        PublicResolver,
    ))
}

/// Runs `builtin.ironhub_search` through the model capability path and returns
/// the URLs the transport was asked to fetch.
async fn search_through_capability_port(
    label: &str,
    catalog_url: &str,
    transport: ScriptedTransport,
) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let services = crate::factory::build_runtime_substrate(
        crate::deployment::local_filesystem_build_input(
            format!("{label}-owner"),
            dir.path().join("standalone"),
        )
        .with_runtime_policy(crate::standalone_runtime_policy().expect("standalone policy"))
        .with_ironhub_manifest_url(manifest_url(catalog_url))
        .with_network_http_egress_for_test(policy_enforcing_network(&transport)),
    )
    .await
    .expect("services build");
    let run_context = run_context(label).await;
    let user_id = UserId::new(format!("{label}-user")).expect("user id");
    enable_global_auto_approve_for_run(&services, &run_context, &user_id).await;
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    ensure_thread_for_run(thread_service.as_ref(), &run_context, &user_id).await;
    let wiring = capability_wiring(
        &services,
        thread_service,
        user_id,
        Arc::new(crate::builtin_capability_policy::builtin_capability_policy().expect("policy")),
        Arc::new(UnavailableModelGateway),
        Arc::new(InMemoryLoopHostMilestoneSink::default()),
        None,
        None,
        None,
        None,
        true,
    )
    .expect("capability wiring");
    let port = wiring
        .capability_factory
        .create_capability_port(&run_context)
        .await
        .expect("capability port");
    port.visible_capabilities(VisibleCapabilityRequest {})
        .await
        .expect("visible surface");
    let definition = port
        .tool_definitions()
        .expect("tool definitions")
        .into_iter()
        .find(|definition| {
            definition.capability_id.as_str()
                == ironclaw_extension_manager::ironhub::IRONHUB_SEARCH_CAPABILITY_ID
        })
        .expect("ironhub_search is on the model surface");
    let candidate = port
        .register_provider_tool_call(RegisterProviderToolCallRequest::new(
            provider_tool_call_with_name(
                definition.name.as_str(),
                serde_json::json!({"query": "anything"}),
            ),
        ))
        .await
        .expect("ironhub_search call stages");
    let resolution = port
        .invoke_capability(invocation_for_candidate(&candidate))
        .await;
    // The body is never a valid signed envelope, so the search always ends
    // in a model-visible failure; what matters is how far the download got.
    assert!(
        matches!(
            &resolution,
            Ok(Resolution::Done(outcome))
                if matches!(outcome.verdict, ToolVerdict::RecoverableFailure { .. })
        ),
        "an unsigned catalog must end in a recoverable failure: {resolution:?}"
    );
    transport.urls()
}

#[tokio::test]
async fn capability_path_follows_github_release_redirect_to_githubusercontent() {
    let asset = "https://release-assets.githubusercontent.com/github-production-release-asset/1/manifest.json";
    let transport = ScriptedTransport::new(vec![redirect_to(asset), unsigned_body()]);

    let urls =
        search_through_capability_port("ironhub-redirect-allowed", GITHUB_MANIFEST_URL, transport)
            .await;

    assert_eq!(
        urls,
        vec![GITHUB_MANIFEST_URL.to_string(), asset.to_string()]
    );
}

/// Hops that left the whitelist when the `*.githubusercontent.com` wildcard was
/// replaced by exact hosts, plus an unrelated host.
const DENIED_HOPS: &[&str] = &[
    "https://attacker.example/manifest.json",
    "https://raw.githubusercontent.com/attacker/repo/main/manifest.json",
    "https://gist.githubusercontent.com/attacker/1/raw/manifest.json",
];

#[tokio::test]
async fn capability_path_refuses_redirect_off_the_host_whitelist() {
    for (index, hop) in DENIED_HOPS.iter().enumerate() {
        let transport = ScriptedTransport::new(vec![redirect_to(hop), unsigned_body()]);

        let urls = search_through_capability_port(
            &format!("ironhub-redirect-denied-{index}"),
            GITHUB_MANIFEST_URL,
            transport,
        )
        .await;

        assert_eq!(urls, vec![GITHUB_MANIFEST_URL.to_string()], "hop {hop}");
    }
}

/// The host whitelist is flat, not origin-aware: a catalog on
/// `hub.ironclaw.com` may redirect to a whitelisted githubusercontent host
/// just as a github.com one can.
#[tokio::test]
async fn capability_path_redirect_allowance_does_not_depend_on_the_original_host() {
    let hub_url = "https://hub.ironclaw.com/api/catalog/manifest.json";
    let asset = "https://objects.githubusercontent.com/github-production-release-asset/1/m.json";
    let transport = ScriptedTransport::new(vec![redirect_to(asset), unsigned_body()]);

    let urls =
        search_through_capability_port("ironhub-redirect-hub-origin", hub_url, transport).await;

    assert_eq!(urls, vec![hub_url.to_string(), asset.to_string()]);
}

/// Runs `ironclaw ironhub list` the way the CLI does — straight into the
/// IronHub service through the runtime, outside capability dispatch — and
/// returns the error (the body is never signed) and the fetched URLs.
async fn list_through_cli_path(label: &str, transport: ScriptedTransport) -> (String, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = crate::RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            format!("{label}-owner"),
            dir.path().join("standalone"),
        )
        .with_runtime_policy(crate::standalone_runtime_policy().expect("standalone policy"))
        .with_network_http_egress_for_test(policy_enforcing_network(&transport)),
    )
    .with_ironhub_manifest_url(manifest_url(GITHUB_MANIFEST_URL))
    .with_model_gateway_override(Arc::new(UnavailableModelGateway));
    let runtime = crate::build_reborn_runtime(input)
        .await
        .expect("runtime builds");

    let error = ironclaw_extension_manager::ironhub::execute_reborn_ironhub_command(
        &runtime,
        ironclaw_extension_manager::ironhub::IronHubCommand::List { kind: None },
    )
    .await
    .expect_err("an unsigned catalog never lists");
    runtime.shutdown().await.expect("runtime shuts down");
    (error.to_string(), transport.urls())
}

/// The direct path used to fail before sending anything, because no network
/// policy was staged for its invocation (`network_policy_missing`). It now
/// borrows the same fixed IronHub policy as the capability path.
#[tokio::test]
async fn cli_path_follows_github_release_redirect_under_the_same_whitelist() {
    let asset = "https://release-assets.githubusercontent.com/github-production-release-asset/2/manifest.json";
    let transport = ScriptedTransport::new(vec![redirect_to(asset), unsigned_body()]);

    let (error, urls) = list_through_cli_path("ironhub-cli-allowed", transport).await;

    assert_eq!(
        urls,
        vec![GITHUB_MANIFEST_URL.to_string(), asset.to_string()]
    );
    assert!(
        error.contains("signed manifest verification failed"),
        "the download completed and reached signature checking: {error}"
    );
}

#[tokio::test]
async fn cli_path_refuses_redirect_off_the_host_whitelist() {
    for (index, hop) in DENIED_HOPS.iter().enumerate() {
        let transport = ScriptedTransport::new(vec![redirect_to(hop), unsigned_body()]);

        let (error, urls) =
            list_through_cli_path(&format!("ironhub-cli-denied-{index}"), transport).await;

        assert_eq!(urls, vec![GITHUB_MANIFEST_URL.to_string()], "hop {hop}");
        assert!(
            !error.contains("signed manifest verification failed"),
            "hop {hop} must stop the download, got {error}"
        );
    }
}
