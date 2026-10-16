//! Operator-configured egress endpoints, end to end over a real socket.
//!
//! A channel's `[[channel.egress]]` target may name an endpoint the operator
//! binds in `config.toml` instead of a literal host. This bin drives one such
//! target through every production layer that decides whether the request
//! leaves the process:
//!
//! operator `config.toml` (parsed and shape-checked by `ironclaw_config`)
//! → typed endpoint bindings → the delivery resolver over a deployment-bound
//! `acme-inbox` fixture channel → the policy egress (logical URL check and
//! origin rewrite) → the host-runtime HTTP egress (credential injection)
//! → `ironclaw_network`'s real policy, resolver, and reqwest transport
//! → a TCP listener on 127.0.0.1.
//!
//! The fake is only the far end: a local listener standing in for a sidecar
//! on the operator's private network. Loopback is a private address, so the
//! private-network opt-in is exactly what decides whether the request lands.
//!
//! A new bin rather than an extension of `real_egress_pipeline.rs`: that bin
//! drives `builtin.http` turns through a recording transport, and this path
//! needs a real listener and the channel delivery resolver, neither of which
//! its harness wires.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ironclaw_extension_contracts::channel_adapter::ChannelSurfaces;
use ironclaw_extension_contracts::channel_endpoint::ChannelEgressEndpointId;
use ironclaw_extension_contracts::tool_adapter::{RestrictedEgressError, RestrictedEgressRequest};
use ironclaw_extension_host::channel_egress::{
    HostRuntimeChannelEgressTransport, StaticChannelEgressCredentials,
};
use ironclaw_extension_host::channel_endpoints::{
    InsecureHttpPosture, OperatorChannelEndpoint, OperatorChannelEndpoints, PrivateNetworkPosture,
};
use ironclaw_extension_host::egress::TransportBackedEgressFactory;
use ironclaw_extension_host::test_support::{
    FakeChannelAdapter, FakeEgressFactory, FakeLoader, RecordingDrain,
};
use ironclaw_extension_host::{
    DeploymentChannelBinding, DeploymentChannelRegistry, ExtensionHost, ExtensionHostDeps,
    InstallationRecordStore, RehydratedInstallationRecordStore, SnapshotChannelDeliveryResolver,
};
use ironclaw_extension_registry::{ExtensionManifestRecord, ManifestSource};
use ironclaw_host_api::{
    action::NetworkMethod,
    host_port::{
        HOST_RUNTIME_HTTP_EGRESS_PORT_ID, HostPortCatalog, HostPortCatalogEntry, HostPortId,
    },
    ids::{ExtensionId, InvocationId, SecretHandle, UserId},
    resource::ResourceScope,
};
use ironclaw_product_contracts::delivery::ChannelDeliveryResolver;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const EXTENSION: &str = "acme-inbox";
const RELAY_TOKEN: &str = "itest-relay-token";
const MANIFEST: &str = include_str!("../fixtures/extensions/acme-inbox/manifest.toml");

/// A one-request HTTP listener on loopback that records what reached it.
struct LoopbackRelay {
    port: u16,
    connections: Arc<AtomicUsize>,
    request: tokio::task::JoinHandle<Option<String>>,
}

impl LoopbackRelay {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&connections);
        let request = tokio::spawn(async move {
            let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
            let (mut socket, _) = accepted.ok()?.ok()?;
            seen.fetch_add(1, Ordering::SeqCst);
            let mut raw = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.ok()?;
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                if request_complete(&raw) {
                    break;
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .ok()?;
            Some(String::from_utf8_lossy(&raw).into_owned())
        });
        Self {
            port,
            connections,
            request,
        }
    }
}

/// Headers read and, when a `content-length` is present, the whole body.
fn request_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some(header_end) = text.find("\r\n\r\n") else {
        return false;
    };
    let length = text[..header_end]
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    raw.len() >= header_end + 4 + length
}

/// Parse an operator `config.toml` through the production parser and map its
/// `[channel_endpoints]` into typed bindings the way the binary does. The
/// binary's own mapping (`ironclaw_cli::runtime::channel_endpoints`) is private
/// to the `ironclaw` bin target, so it cannot be called from here; it has its
/// own unit tests, and this copy uses the same public constructors.
fn operator_endpoints(config_toml: &str) -> OperatorChannelEndpoints {
    let config =
        ironclaw_config::RebornConfigFile::parse_text(config_toml, Path::new("config.toml"))
            .expect("operator config parses");
    let mut endpoints = OperatorChannelEndpoints::new();
    for (extension_id, entries) in config.channel_endpoints.unwrap_or_default() {
        for (endpoint_id, entry) in entries {
            let endpoint = OperatorChannelEndpoint::parse(
                &entry.url,
                if entry.allow_private_network {
                    PrivateNetworkPosture::Allowed
                } else {
                    PrivateNetworkPosture::Denied
                },
                if entry.allow_insecure_http {
                    InsecureHttpPosture::Allowed
                } else {
                    InsecureHttpPosture::Denied
                },
            )
            .expect("operator endpoint is valid");
            endpoints
                .insert(
                    ExtensionId::new(extension_id.as_str()).expect("extension id"),
                    ChannelEgressEndpointId::new(endpoint_id.as_str()).expect("endpoint id"),
                    endpoint,
                )
                .expect("unique binding");
        }
    }
    endpoints
}

/// The production delivery resolver over the deployment-bound fixture
/// channel, with the real host-runtime egress and network stack beneath it.
async fn delivery_resolver(endpoints: OperatorChannelEndpoints) -> SnapshotChannelDeliveryResolver {
    let catalog = HostPortCatalog::new(vec![HostPortCatalogEntry::new(
        HostPortId::new(HOST_RUNTIME_HTTP_EGRESS_PORT_ID).expect("port id"),
    )])
    .expect("catalog");
    let record = ExtensionManifestRecord::from_toml(
        MANIFEST,
        ManifestSource::HostBundled,
        &catalog,
        None,
        &ironclaw_extension_registry::default_host_api_contract_registry()
            .expect("contract registry"),
        None,
    )
    .expect("acme-inbox fixture parses");
    let channel = Arc::new(FakeChannelAdapter::default());
    let registry = DeploymentChannelRegistry::try_new([DeploymentChannelBinding::new(
        Arc::new(record.resolved().clone()),
        ChannelSurfaces::default().with_delivery(channel),
    )
    .expect("deployment binding")])
    .expect("deployment registry");

    let host_egress = ironclaw_host_runtime::HostRuntimeServices::new(
        Arc::new(ironclaw_extension_registry::ExtensionRegistry::new()),
        Arc::new(ironclaw_filesystem::DiskFilesystem::new()),
        Arc::new(ironclaw_resources::InMemoryResourceGovernor::new()),
        Arc::new(ironclaw_authorization::GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        ironclaw_host_runtime::CapabilitySurfaceVersion::new("operator-endpoints-v1")
            .expect("surface version"),
    )
    .with_secret_store(Arc::new(ironclaw_secrets::SecretStore::ephemeral()))
    .try_with_host_http_egress(ironclaw_network::PolicyNetworkHttpEgress::new(
        ironclaw_network::ReqwestNetworkTransport::new(Duration::from_secs(5)),
    ))
    .expect("host HTTP egress wires")
    .host_runtime_http_egress_port()
    .expect("host HTTP egress port");
    let credentials = Arc::new(StaticChannelEgressCredentials::new(vec![(
        EXTENSION.to_string(),
        "acme_inbox_relay_token".to_string(),
        RELAY_TOKEN.to_string().into(),
    )]));
    let scope = ResourceScope::local_default(
        UserId::new("operator-endpoints-user").expect("user"),
        InvocationId::new(),
    )
    .expect("scope");
    let transport = Arc::new(HostRuntimeChannelEgressTransport::new(
        host_egress,
        credentials,
        scope,
    ));

    let store = Arc::new(RehydratedInstallationRecordStore::default());
    let host = ExtensionHost::new(ExtensionHostDeps {
        store: store as Arc<dyn InstallationRecordStore>,
        loader: Arc::new(FakeLoader {
            bindings: Default::default(),
            load_calls: Arc::new(AtomicUsize::new(0)),
            fail_load: false,
        }),
        drain: Arc::new(RecordingDrain::default()),
        egress: Arc::new(FakeEgressFactory),
        reserved_capability_ids: Default::default(),
        reserved_ingress_routes: Default::default(),
        hook_deadline: Duration::from_secs(5),
        linked_sessions: ironclaw_extension_host::LinkedSessionStore::unavailable(),
        linked_accounts: Arc::new(ironclaw_extension_host::UnavailableLinkedAccountResolution),
        admin_secrets: None,
    })
    .await;
    SnapshotChannelDeliveryResolver::new(
        host.snapshot_watch(),
        Arc::new(TransportBackedEgressFactory::new(
            transport,
            Arc::new(endpoints),
        )),
    )
    .with_deployment_channels(Arc::new(registry))
}

/// What the fixture's delivery half would send: a POST to the logical URL.
fn relay_send() -> RestrictedEgressRequest {
    RestrictedEgressRequest {
        method: NetworkMethod::Post,
        url: "https://relay.channel-endpoint.invalid/v1/messages".to_string(),
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: Some(br#"{"text":"hello"}"#.to_vec()),
        credential: Some(SecretHandle::new("acme_inbox_relay_token").expect("handle")),
        body_credentials: Vec::new(),
    }
}

#[tokio::test]
async fn an_opted_in_endpoint_delivers_to_the_operator_address_with_the_credential() {
    let relay = LoopbackRelay::start().await;
    let resolver = delivery_resolver(operator_endpoints(&format!(
        "[channel_endpoints.acme-inbox.relay]\nurl = \"http://127.0.0.1:{}\"\n\
         allow_private_network = true\nallow_insecure_http = true\n",
        relay.port
    )))
    .await;
    let delivery = resolver
        .resolve_channel_delivery(EXTENSION)
        .expect("the deployment-bound channel resolves");
    assert!(
        delivery.declared_egress_hosts.is_empty(),
        "an operator endpoint is never offered as an enrollable host"
    );

    let response = delivery
        .egress
        .send(relay_send())
        .await
        .expect("the opted-in endpoint is reachable");
    assert_eq!(response.status, 200);

    let request = relay
        .request
        .await
        .expect("listener task")
        .expect("the request reached the operator's address");
    let request_line = request.lines().next().unwrap_or_default();
    assert_eq!(request_line, "POST /v1/messages HTTP/1.1");
    let lowered = request.to_ascii_lowercase();
    assert!(
        lowered.contains(&format!("authorization: bearer {RELAY_TOKEN}")),
        "the declared credential is injected host-side: {request}"
    );
    assert!(request.ends_with(r#"{"text":"hello"}"#), "{request}");
}

#[tokio::test]
async fn without_the_private_network_opt_in_the_request_never_leaves_the_host() {
    let relay = LoopbackRelay::start().await;
    let resolver = delivery_resolver(operator_endpoints(&format!(
        "[channel_endpoints.acme-inbox.relay]\nurl = \"https://127.0.0.1:{}\"\n",
        relay.port
    )))
    .await;
    let delivery = resolver
        .resolve_channel_delivery(EXTENSION)
        .expect("the deployment-bound channel resolves");

    let error = delivery
        .egress
        .send(relay_send())
        .await
        .expect_err("a loopback address without the opt-in is refused");
    assert!(
        matches!(error, RestrictedEgressError::PolicyDenied),
        "{error:?}"
    );
    relay.request.abort();
    assert_eq!(
        relay.connections.load(Ordering::SeqCst),
        0,
        "the listener was never contacted"
    );
}

#[tokio::test]
async fn an_endpoint_the_operator_never_bound_is_refused_before_transport() {
    let resolver = delivery_resolver(OperatorChannelEndpoints::new()).await;
    let delivery = resolver
        .resolve_channel_delivery(EXTENSION)
        .expect("the deployment-bound channel resolves");
    let error = delivery
        .egress
        .send(relay_send())
        .await
        .expect_err("an unbound endpoint is refused");
    assert!(
        matches!(error, RestrictedEgressError::PolicyDenied),
        "{error:?}"
    );
}
