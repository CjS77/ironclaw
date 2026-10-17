//! Intake-mode channel profile: the delivery profile plus the invented
//! `acme-intake` fixture channel and the two intake read tools.
//!
//! The fixture's manifest (`tests/fixtures/extensions/acme-intake/`) declares
//! `[channel.intake]`, so the production channel host assembly registers the
//! intake sink for it instead of the product workflow. Its ingress adapter is
//! the scripted [`AcmeIntakeAdapter`] below — the one vendor half a test has
//! to supply — bound through the same `ChannelExtensionBinding` seam the
//! binary uses for real packages.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_extension_contracts::channel_adapter::{
    ChannelError, ChannelIngress, ChannelSurfaces, InboundOutcome, NormalizedInboundMessage,
    OwnerActivity, ProductTriggerReason, VerifiedInbound,
};
use ironclaw_extension_contracts::external::{
    ExternalActorRef, ExternalConversationRef, ExternalEventId,
};
use ironclaw_extension_contracts::tool_adapter::RestrictedEgress;
use ironclaw_host_api::ids::CapabilityId;

use super::super::HarnessResult;
use super::super::options::ToolsProfile;

pub(crate) const ACME_INTAKE_EXTENSION_ID: &str = "acme-intake";
pub(crate) const ACME_INTAKE_ADMIN_GROUP: &str = "vendor.acme-intake";
pub(crate) const ACME_INTAKE_SECRET_HANDLE: &str = "acme_intake_signing_secret";
const ACME_INTAKE_MANIFEST: &str =
    include_str!("../../../../fixtures/extensions/acme-intake/manifest.toml");

/// The acme-intake vendor protocol, invented for this suite:
/// `{"type":"message", "event_id", "conversation", "user", "name"?, "text",
/// "reply_context"?}` is an outsider's message; `{"type":"owner_reply", "conversation",
/// "occurred_at"}` (RFC 3339) is the account owner replying from their own
/// device.
/// Anything else is ignored.
pub(crate) struct AcmeIntakeAdapter;

#[async_trait]
impl ChannelIngress for AcmeIntakeAdapter {
    async fn receive(
        &self,
        request: VerifiedInbound<'_>,
        _egress: &dyn RestrictedEgress,
    ) -> Result<InboundOutcome, ChannelError> {
        let value: serde_json::Value =
            serde_json::from_slice(request.body).map_err(|error| ChannelError::Parse {
                reason: error.to_string(),
            })?;
        let field = |name: &str| {
            value[name]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ChannelError::Parse {
                    reason: format!("acme-intake payload is missing `{name}`"),
                })
        };
        let parse = |error: ironclaw_host_api::product_adapter_error::ProductAdapterError| {
            ChannelError::Parse {
                reason: error.to_string(),
            }
        };
        match value["type"].as_str() {
            Some("message") => Ok(InboundOutcome::Messages(vec![NormalizedInboundMessage {
                actor: ExternalActorRef::new(
                    "acme_contact",
                    field("user")?,
                    value["name"].as_str(),
                )
                .map_err(parse)?,
                conversation: ExternalConversationRef::new(
                    None,
                    field("conversation")?,
                    None,
                    None,
                )
                .map_err(parse)?,
                event_id: ExternalEventId::new(field("event_id")?).map_err(parse)?,
                text: field("text")?,
                trigger: ProductTriggerReason::DirectChat,
                attachments: Vec::new(),
                conversation_context: None,
                reply_context: value["reply_context"]
                    .as_str()
                    .map(|context| context.as_bytes().to_vec()),
            }])),
            Some("owner_reply") => Ok(InboundOutcome::OwnerActivity(OwnerActivity {
                conversation: ExternalConversationRef::new(
                    None,
                    field("conversation")?,
                    None,
                    None,
                )
                .map_err(parse)?,
                occurred_at: chrono::DateTime::parse_from_rfc3339(&field("occurred_at")?)
                    .map_err(|error| ChannelError::Parse {
                        reason: error.to_string(),
                    })?
                    .with_timezone(&chrono::Utc),
            })),
            _ => Ok(InboundOutcome::Ignore),
        }
    }
}

fn acme_intake_channel_binding() -> ironclaw_composition::ChannelExtensionBinding {
    ironclaw_composition::ChannelExtensionBinding {
        extension_id: ironclaw_host_api::ids::ExtensionId::from_trusted(
            ACME_INTAKE_EXTENSION_ID.to_string(),
        ),
        surfaces: ChannelSurfaces::default().with_ingress(Arc::new(AcmeIntakeAdapter)),
        preference_target_codec: None,
        outbound_target_provider: None,
        first_party_initializer: None,
        registration_document_path: None,
    }
}

fn acme_intake_bundle() -> ironclaw_extension_host::FirstPartyPackageBundle {
    ironclaw_extension_host::FirstPartyPackageBundle {
        id: ACME_INTAKE_EXTENSION_ID.to_string(),
        display_name: "Acme Intake".to_string(),
        manifest_toml: ACME_INTAKE_MANIFEST.to_string(),
        assets: vec![ironclaw_extension_host::FirstPartyPackageAsset {
            path: "manifest.toml".to_string(),
            bytes: ACME_INTAKE_MANIFEST.as_bytes().to_vec(),
        }],
        onboarding: None,
        oauth_setup: None,
        trust_effects: None,
        search_aliases: Vec::new(),
    }
}

/// [`super::extension::extension_delivery_tools_profile`] plus the
/// acme-intake deployment channel and the `builtin.intake.*` read tools.
/// `options` is mutated, never rebuilt, so the delivery profile's channel
/// bindings, native factories, and recording egress survive.
pub(crate) fn intake_channel_tools_profile() -> HarnessResult<ToolsProfile> {
    let mut profile = super::extension::extension_delivery_tools_profile()?;
    for id in [
        ironclaw_host_runtime::INTAKE_LIST_CAPABILITY_ID,
        ironclaw_host_runtime::INTAKE_READ_CAPABILITY_ID,
    ] {
        profile.capability_ids.push(CapabilityId::new(id)?);
    }
    profile.options = profile
        .options
        .with_channel_extension_binding(acme_intake_channel_binding());
    profile
        .options
        .extra_first_party_bundles
        .push(acme_intake_bundle());
    Ok(profile)
}
