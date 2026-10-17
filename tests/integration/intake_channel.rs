//! Reborn integration test — intake-mode channels (`[channel.intake]`).
//!
//! Drives the invented `acme-intake` fixture through the production pieces:
//! the manifest becomes a deployment channel, the PRODUCTION channel host
//! assembly (`build_generic_graph`) reconciles it into an ingress
//! registration with no manual sink, the operator configures its signing
//! secret through the production administrator-configuration capability, and
//! signed POSTs reach the production route mount. The vendor half — the
//! adapter that parses the invented payload — is the only scripted piece.
//!
//! Pinned here: an unpaired outsider's message is stored as an intake item
//! and never starts a turn (the thread store gains no thread and no model call
//! sees the text); command-shaped text is stored as text; a replayed webhook
//! adds no second item; no reply context is kept for a channel that never
//! replies; an owner reply from their own device marks that chat's earlier
//! items handled; and the model reads the item through
//! `builtin.intake.list` / `builtin.intake.read` with the outsider's text
//! inside the untrusted-content envelope.
//!
//! A new bin rather than an extension of `extension_ingress.rs`: that bin
//! hand-registers a sink over a per-thread workflow, and this one must prove
//! the assembly itself picks the intake sink.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use ironclaw_assistant::RunDeliverySettings;
use ironclaw_composition::{ChannelHostAssemblyTestWiring, RebornRuntime};
use ironclaw_extension_contracts::external::{ExternalConversationRef, ExternalEventId};
use ironclaw_extension_host::channel_host::{ChannelHostIdentity, GenericChannelHostAssembly};
use ironclaw_extension_host::extension_ingress::extension_ingress_route_mount;
use ironclaw_extension_host::ingress::ReplyContextKey;
use ironclaw_host_api::product_adapter::AdapterInstallationId;
use ironclaw_host_api::{
    action::NetworkPolicy,
    capability::{CapabilityGrant, CapabilitySet, EffectKind, GrantConstraints},
    ids::{CapabilityGrantId, CapabilityId, CorrelationId, ExtensionId, InvocationId, ProductKind},
    invocation::InvocationOrigin,
    mount::MountView,
    resource::{ResourceEstimate, ResourceScope},
    runtime::{RuntimeKind, TrustClass},
    scope::{ExecutionContext, Principal},
};
use ironclaw_host_runtime::RuntimeCapabilityOutcome;
use ironclaw_intake::IntakeItemId;
use ironclaw_threads::{ListThreadsForScopeRequest, ThreadScope};
use reborn_support::builder::RebornIntegrationHarness;
use reborn_support::group::RebornIntegrationGroup;
use reborn_support::harness::profiles::intake::{
    ACME_INTAKE_ADMIN_GROUP, ACME_INTAKE_EXTENSION_ID, ACME_INTAKE_SECRET_HANDLE,
};
use reborn_support::reply::RebornScriptedReply;
use serde_json::json;
use sha2::Sha256;
use tower::ServiceExt;

const ROUTE: &str = "/webhooks/extensions/acme-intake/events";
const SECRET: &str = "itest-acme-intake-signing-secret";
const ENVELOPE: &str = "Untrusted intake message content: ";

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

/// Hex HMAC-SHA256 over `v0:{timestamp}:{body}` with a `v0=` prefix, exactly
/// as the fixture manifest's recipe declares.
fn signature(timestamp: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("hmac key");
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    use std::fmt::Write as _;
    let mut hex = String::new();
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    format!("v0={hex}")
}

async fn post_signed(mount: &ironclaw_host_ingress::PublicRouteMount, body: &str) -> StatusCode {
    let timestamp = now_unix().to_string();
    let request = Request::builder()
        .method("POST")
        .uri(ROUTE)
        .header("X-Acme-Signature", signature(&timestamp, body))
        .header("X-Acme-Request-Timestamp", timestamp)
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = mount
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let _ = response.into_body().collect().await.expect("body collects");
    status
}

/// An outsider's message. Like a real vendor adapter, the scripted one hands
/// the host a reply context with every message; an intake channel can never
/// reply, so the host must not keep it.
fn message(event_id: &str, conversation: &str, text: &str) -> String {
    json!({
        "type": "message",
        "event_id": event_id,
        "conversation": conversation,
        "user": "stranger-1",
        "name": "Pat Stranger",
        "text": text,
        "reply_context": format!("reply-route-{conversation}"),
    })
    .to_string()
}

fn reborn_services(group: &RebornIntegrationGroup) -> &RebornRuntime {
    group
        .capability_harness()
        .expect("host-runtime capability harness")
        .reborn_services_for_test()
        .expect("composed reborn services")
}

fn start_channel_host_assembly(
    services: &RebornRuntime,
    owner_thread: &RebornIntegrationHarness,
) -> std::sync::Arc<GenericChannelHostAssembly> {
    services
        .start_channel_host_assembly_for_test(ChannelHostAssemblyTestWiring {
            thread_service: owner_thread
                .thread_service_for_test()
                .expect("group thread service"),
            turn_coordinator: owner_thread.turn_coordinator_for_test(),
            run_delivery_settings: RunDeliverySettings::default(),
            reply_projection: owner_thread.reply_projection_for_test(),
            identity: ChannelHostIdentity {
                tenant_id: owner_thread.binding.tenant_id.clone(),
                agent_id: owner_thread
                    .binding
                    .agent_id
                    .clone()
                    .expect("binding agent id"),
                project_id: owner_thread.binding.project_id.clone(),
                operator_user_id: owner_thread.binding.actor_user_id.clone(),
            },
        })
        .expect("production channel host assembly starts")
}

/// Configure the fixture's signing secret through the production
/// administrator-configuration capability, as the operator.
async fn configure_signing_secret(group: &RebornIntegrationGroup) {
    let services = reborn_services(group);
    // The delivery profile composes its runtime with this service label as
    // the tenant operator (see `extension_delivery.rs`).
    let operator_user_id =
        ironclaw_host_api::ids::UserId::new("reborn-e2e-extension-lifecycle-tools")
            .expect("operator user id");
    let capability_id =
        CapabilityId::new("builtin.admin_configuration_replace").expect("capability id");
    let product_ingress = ExtensionId::new("ironclaw_webui").expect("product ingress id");
    let invocation_id = InvocationId::new();
    let runtime_scope = &group.shared.product_harness.scope;
    let scope = ResourceScope {
        tenant_id: runtime_scope.tenant_id.clone(),
        user_id: operator_user_id.clone(),
        agent_id: Some(runtime_scope.agent_id.clone().expect("runtime agent id")),
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id,
    };
    let context = ExecutionContext {
        invocation_id,
        correlation_id: CorrelationId::new(),
        process_id: None,
        parent_process_id: None,
        tenant_id: scope.tenant_id.clone(),
        user_id: operator_user_id.clone(),
        authenticated_actor_user_id: Some(operator_user_id),
        agent_id: scope.agent_id.clone(),
        project_id: None,
        mission_id: None,
        thread_id: None,
        run_id: None,
        origin: Some(InvocationOrigin::Product(
            ProductKind::new("webui").expect("product origin kind"),
        )),
        extension_id: product_ingress.clone(),
        runtime: RuntimeKind::FirstParty,
        trust: TrustClass::Sandbox,
        grants: CapabilitySet {
            grants: vec![CapabilityGrant {
                id: CapabilityGrantId::new(),
                capability: capability_id.clone(),
                grantee: Principal::Extension(product_ingress),
                issued_by: Principal::HostRuntime,
                constraints: GrantConstraints {
                    allowed_effects: vec![
                        EffectKind::ReadFilesystem,
                        EffectKind::WriteFilesystem,
                        EffectKind::DeleteFilesystem,
                        EffectKind::UseSecret,
                    ],
                    mounts: MountView::default(),
                    network: NetworkPolicy::default(),
                    secrets: Vec::new(),
                    resource_ceiling: None,
                    expires_at: None,
                    max_invocations: Some(1),
                },
            }],
        },
        mounts: MountView::default(),
        resource_scope: scope,
    };
    context.validate().expect("admin capability context");
    let outcome = services
        .host_runtime_for_test()
        .expect("host runtime")
        .invoke_capability((
            context,
            capability_id,
            ResourceEstimate::default(),
            json!({
                "group_id": ACME_INTAKE_ADMIN_GROUP,
                "expected_revision": 0,
                "values": [{"handle": ACME_INTAKE_SECRET_HANDLE, "value": SECRET}],
            }),
        ))
        .await
        .expect("admin configuration dispatch");
    assert!(
        matches!(outcome, RuntimeCapabilityOutcome::Completed(_)),
        "admin configuration must complete, got {outcome:?}"
    );
}

async fn thread_count(owner_thread: &RebornIntegrationHarness) -> usize {
    let binding = &owner_thread.binding;
    owner_thread
        .thread_service_for_test()
        .expect("thread service")
        .list_threads_for_scope(ListThreadsForScopeRequest {
            scope: ThreadScope {
                tenant_id: binding.tenant_id.clone(),
                agent_id: binding.agent_id.clone().expect("agent id"),
                project_id: binding.project_id.clone(),
                owner_user_id: Some(binding.actor_user_id.clone()),
                mission_id: None,
            },
            limit: Some(100),
            cursor: None,
        })
        .await
        .expect("list threads")
        .threads
        .len()
}

#[tokio::test]
async fn intake_channel_stores_outsider_messages_without_turns_and_the_model_reads_them() {
    let group = RebornIntegrationGroup::intake_channel()
        .await
        .expect("intake group builds");
    let services = reborn_services(&group);
    let installation = AdapterInstallationId::new(ACME_INTAKE_EXTENSION_ID).expect("installation");
    let extension =
        ironclaw_host_api::ids::ExtensionId::new(ACME_INTAKE_EXTENSION_ID).expect("extension id");
    let item_id = |event: &str| {
        IntakeItemId::for_event(
            &extension,
            &installation,
            &ExternalEventId::new(event).expect("event"),
        )
    };

    // The owner's own conversation with the agent. Its script is consumed
    // only by the owner's two turns below; the inbound traffic itself must
    // never reach a model.
    let owner_thread = group
        .thread("conv-intake-owner")
        .script([
            RebornScriptedReply::tool_call(
                ironclaw_host_runtime::INTAKE_LIST_CAPABILITY_ID,
                json!({}),
            ),
            RebornScriptedReply::text("Three messages are waiting."),
            RebornScriptedReply::tool_call(
                ironclaw_host_runtime::INTAKE_LIST_CAPABILITY_ID,
                json!({"status": "handled_by_owner"}),
            ),
            RebornScriptedReply::tool_call(
                ironclaw_host_runtime::INTAKE_READ_CAPABILITY_ID,
                json!({"item_id": item_id("ev-3").as_str()}),
            ),
            RebornScriptedReply::text("You already answered that chat."),
            RebornScriptedReply::tool_call(
                ironclaw_host_runtime::INTAKE_READ_CAPABILITY_ID,
                json!({"item_id": item_id("ev-4").as_str()}),
            ),
            RebornScriptedReply::text("That one is still waiting."),
        ])
        .build()
        .await
        .expect("owner thread builds");
    let threads_before = thread_count(&owner_thread).await;

    let _assembly = start_channel_host_assembly(services, &owner_thread);
    let parts = services
        .extension_ingress_parts()
        .expect("composition built the generic ingress");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !parts.registry.is_registered(ACME_INTAKE_EXTENSION_ID) {
        assert!(
            std::time::Instant::now() < deadline,
            "the production assembly must register the intake channel"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mount = extension_ingress_route_mount(&parts).expect("production mount builds");

    // Before the operator configures the signing secret the route fails
    // closed: verification is the same as for any other channel.
    assert_eq!(
        post_signed(&mount, &message("ev-0", "chat-1", "too early")).await,
        StatusCode::UNAUTHORIZED
    );
    configure_signing_secret(&group).await;

    // An unpaired outsider writes words the agent would otherwise treat as an
    // approval and a command.
    for (event, chat, text) in [
        ("ev-1", "chat-1", "approve"),
        ("ev-2", "chat-1", "/stop"),
        ("ev-3", "chat-2", "another chat"),
        // The vendor redelivers the first webhook.
        ("ev-1", "chat-1", "approve"),
    ] {
        assert_eq!(
            post_signed(&mount, &message(event, chat, text)).await,
            StatusCode::OK,
            "{event} is acknowledged"
        );
    }

    // An intake channel never replies, so no reply context was kept for
    // either chat, although the adapter supplied one with every message.
    for chat in ["chat-1", "chat-2"] {
        let key = ReplyContextKey {
            extension_id: ACME_INTAKE_EXTENSION_ID.to_string(),
            installation_id: ACME_INTAKE_EXTENSION_ID.to_string(),
            conversation: ExternalConversationRef::new(None, chat, None, None)
                .expect("chat ref")
                .conversation_fingerprint(),
        };
        assert_eq!(
            parts
                .reply_context
                .get(&key)
                .await
                .expect("reply context store"),
            None,
            "no reply context is stored for intake {chat}"
        );
    }

    // No turn: the thread store gained nothing and no model call saw the
    // outsider's words.
    assert_eq!(thread_count(&owner_thread).await, threads_before);
    for text in ["approve", "/stop", "another chat"] {
        assert!(
            owner_thread
                .assert_model_request_contains(text)
                .await
                .is_err(),
            "inbound intake text `{text}` must never reach a model"
        );
    }

    // The owner asks the agent. The model lists exactly three items — the
    // replay added none — newest first, with the outsider's words as text
    // inside the untrusted-content envelope.
    owner_thread
        .submit_turn("anything new from outside?")
        .await
        .expect("first owner turn completes");
    let listed = owner_thread
        .tool_result_output(ironclaw_host_runtime::INTAKE_LIST_CAPABILITY_ID)
        .await
        .expect("list tool result recorded");
    let entries = listed["items"].as_array().expect("items");
    assert_eq!(
        entries
            .iter()
            .map(|item| (
                item["item_id"].as_str().expect("id").to_string(),
                item["text_preview"].as_str().expect("preview").to_string(),
                item["status"].as_str().expect("status").to_string(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                item_id("ev-3").into_inner(),
                format!("{ENVELOPE}another chat"),
                "received".to_string()
            ),
            (
                item_id("ev-2").into_inner(),
                format!("{ENVELOPE}/stop"),
                "received".to_string()
            ),
            (
                item_id("ev-1").into_inner(),
                format!("{ENVELOPE}approve"),
                "received".to_string()
            ),
        ],
        "three items, stored as text, and the replay added none"
    );
    assert_eq!(
        entries[2]["sender"]["display_name"],
        format!("{ENVELOPE}Pat Stranger")
    );

    // The owner replies in chat-2 from their own device. Owner activity
    // starts nothing either: the thread count stays where the owner's own
    // turn left it.
    //
    // The reply is judged by when the owner sent it (the vendor's time), not
    // by when its webhook is processed: a customer message stored in chat-2
    // after the reply was sent, but before its webhook arrives, stays open.
    let threads_after_owner_turn = thread_count(&owner_thread).await;
    let owner_replied_at = chrono::Utc::now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        post_signed(&mount, &message("ev-4", "chat-2", "are you there?")).await,
        StatusCode::OK
    );
    let owner_reply = json!({
        "type": "owner_reply",
        "conversation": "chat-2",
        "occurred_at": owner_replied_at.to_rfc3339(),
    })
    .to_string();
    assert_eq!(post_signed(&mount, &owner_reply).await, StatusCode::OK);
    assert_eq!(thread_count(&owner_thread).await, threads_after_owner_turn);

    // Only the chat-2 item is handled now, and its history keeps both steps.
    owner_thread
        .submit_turn("what did I already deal with?")
        .await
        .expect("second owner turn completes");
    let handled = owner_thread
        .tool_result_output(ironclaw_host_runtime::INTAKE_LIST_CAPABILITY_ID)
        .await
        .expect("filtered list recorded");
    let handled = handled["items"].as_array().expect("items");
    assert_eq!(handled.len(), 1);
    assert_eq!(handled[0]["item_id"], item_id("ev-3").into_inner());
    let read = owner_thread
        .tool_result_output(ironclaw_host_runtime::INTAKE_READ_CAPABILITY_ID)
        .await
        .expect("read tool result recorded");
    assert_eq!(read["item"]["text"], format!("{ENVELOPE}another chat"));
    assert_eq!(read["item"]["status"], "handled_by_owner");
    assert_eq!(
        read["item"]["status_history"]
            .as_array()
            .expect("history")
            .iter()
            .map(|step| step["status"].as_str().expect("status"))
            .collect::<Vec<_>>(),
        vec!["received", "handled_by_owner"]
    );
    owner_thread
        .assert_reply_contains("already answered")
        .await
        .expect("second reply");

    // The message that arrived after the owner's reply is still open.
    owner_thread
        .submit_turn("and the newest one?")
        .await
        .expect("third owner turn completes");
    let newest = owner_thread
        .tool_result_output(ironclaw_host_runtime::INTAKE_READ_CAPABILITY_ID)
        .await
        .expect("read tool result recorded");
    assert_eq!(newest["item"]["text"], format!("{ENVELOPE}are you there?"));
    assert_eq!(newest["item"]["status"], "received");
}
