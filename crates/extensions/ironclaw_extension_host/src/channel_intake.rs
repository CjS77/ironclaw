//! The inbound sink for intake-mode channels (`[channel.intake]`).
//!
//! A verified message on an intake channel is stored as an intake item for
//! the deployment owner and acknowledged. Nothing here reaches the product
//! workflow: no conversation binding, no actor resolution, no pairing gate,
//! no command parsing, and never a turn. Verification is unchanged — the
//! router runs the channel's recipe before this sink is called.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_host_api::ids::ExtensionId;
use ironclaw_host_api::product_adapter::AdapterInstallationId;
use ironclaw_intake::{
    IntakeAttachmentMetadata, IntakeError, IntakeOwner, IntakeRecorder, MarkChatHandledRequest,
    RecordIntakeItemRequest, RecordIntakeOutcome,
};

use crate::channel_host::ChannelHostIdentity;
use crate::extension_ingress::ChannelIngressRegistration;
use crate::ingress::{
    InboundAdmission, InboundAdmissionAck, InboundSink, InboundSinkError, IngressConfigurationPort,
    IngressSecretsPort, OwnerActivityAdmission,
};

/// The ingress registration of one intake channel: the channel's own
/// verification secrets and configuration, and a sink that stores items for
/// the deployment operator. In v1 the operator owns every intake item — the
/// same user the operator actor policy resolves verified actors to.
pub(crate) fn intake_channel_registration(
    store: Arc<dyn IntakeRecorder>,
    identity: &ChannelHostIdentity,
    extension_id: &str,
    secrets: Arc<dyn IngressSecretsPort>,
    configuration: Arc<dyn IngressConfigurationPort>,
) -> Result<ChannelIngressRegistration, String> {
    let extension_id =
        ExtensionId::new(extension_id).map_err(|error| format!("invalid extension id: {error}"))?;
    Ok(ChannelIngressRegistration {
        secrets,
        configuration,
        sink: Arc::new(IntakeChannelInboundSink::new(
            store,
            IntakeOwner {
                tenant_id: identity.tenant_id.clone(),
                user_id: identity.operator_user_id.clone(),
            },
            extension_id,
        )),
        drain: None,
        // An intake channel never replies, so no reply context is kept.
        keeps_reply_context: false,
    })
}

/// Stores every admitted message of one intake channel as an intake item
/// owned by `owner`, and marks a chat's items handled on owner activity.
pub(crate) struct IntakeChannelInboundSink {
    store: Arc<dyn IntakeRecorder>,
    owner: IntakeOwner,
    extension_id: ExtensionId,
}

impl IntakeChannelInboundSink {
    pub(crate) fn new(
        store: Arc<dyn IntakeRecorder>,
        owner: IntakeOwner,
        extension_id: ExtensionId,
    ) -> Self {
        Self {
            store,
            owner,
            extension_id,
        }
    }

    fn installation_id(
        &self,
        extension_id: &str,
        installation_id: &str,
    ) -> Result<AdapterInstallationId, InboundSinkError> {
        if extension_id != self.extension_id.as_str() {
            return Err(permanent(
                "intake sink received another extension's request".to_string(),
            ));
        }
        AdapterInstallationId::new(installation_id)
            .map_err(|error| permanent(format!("invalid installation id: {error}")))
    }
}

#[async_trait]
impl InboundSink for IntakeChannelInboundSink {
    async fn admit(
        &self,
        admission: InboundAdmission,
    ) -> Result<InboundAdmissionAck, InboundSinkError> {
        let installation_id =
            self.installation_id(&admission.extension_id, &admission.installation_id)?;
        let message = admission.message;
        let mut attachments_dropped = false;
        let mut attachments = Vec::with_capacity(message.attachments.len());
        // Metadata only: the bytes the adapter fetched are dropped here.
        for attachment in &message.attachments {
            match IntakeAttachmentMetadata::new(
                attachment.id.clone(),
                attachment.mime_type.clone(),
                attachment.filename.clone(),
                attachment.bytes.len() as u64,
            ) {
                Ok(metadata) => attachments.push(metadata),
                Err(error) => {
                    tracing::debug!(%error, "dropping malformed intake attachment metadata");
                    attachments_dropped = true;
                }
            }
        }
        let outcome = self
            .store
            .record(RecordIntakeItemRequest {
                owner: self.owner.clone(),
                extension_id: self.extension_id.clone(),
                installation_id,
                external_event_id: message.event_id,
                chat_ref: message.conversation,
                contact_ref: message.actor,
                text: message.text,
                attachments,
                attachments_dropped,
                received_at: Utc::now(),
            })
            .await
            .map_err(sink_error)?;
        Ok(match outcome {
            RecordIntakeOutcome::Created(_) => InboundAdmissionAck::Accepted,
            RecordIntakeOutcome::Duplicate(_) => InboundAdmissionAck::Duplicate,
        })
    }

    async fn owner_activity(
        &self,
        admission: OwnerActivityAdmission,
    ) -> Result<(), InboundSinkError> {
        let installation_id =
            self.installation_id(&admission.extension_id, &admission.installation_id)?;
        let handled = self
            .store
            .mark_chat_handled_by_owner(MarkChatHandledRequest {
                owner: self.owner.clone(),
                extension_id: self.extension_id.clone(),
                installation_id,
                chat_ref: admission.activity.conversation,
                // The vendor's time of the owner's action, compared with the
                // host's receive time of each item. Clamped to the host's
                // clock on arrival, so a future vendor time can never enter
                // an item's history.
                occurred_at: admission.activity.occurred_at.min(Utc::now()),
            })
            .await
            .map_err(sink_error)?;
        tracing::debug!(
            extension_id = %self.extension_id,
            handled,
            "owner activity marked intake items handled"
        );
        Ok(())
    }
}

/// Storage faults are retryable (the vendor redelivers): a backend outage, and
/// a record that names another owner, which means the per-user mount is
/// misconfigured until an operator fixes it. Everything else is
/// deterministic and is acknowledged and discarded, so an ordered vendor
/// queue is not held behind it: a request the store refuses, and a stored
/// record that cannot be decoded or encoded, which a replay of the same
/// webhook would hit again.
fn sink_error(error: IntakeError) -> InboundSinkError {
    let retryable = matches!(
        error,
        IntakeError::Backend { .. } | IntakeError::ForeignRecord
    );
    // The router logs this reason (a permanent failure at warn). A
    // serialization cause can quote stored outsider content, so it is dropped
    // and only the fixed `Display` is kept; a backend cause names storage, not
    // content, and is kept.
    let reason = match &error {
        IntakeError::Backend { reason } => format!("{error}: {reason}"),
        IntakeError::Serialization { .. }
        | IntakeError::InvalidRequest { .. }
        | IntakeError::Conflict
        | IntakeError::ForeignRecord => error.to_string(),
    };
    InboundSinkError { retryable, reason }
}

fn permanent(reason: String) -> InboundSinkError {
    InboundSinkError {
        retryable: false,
        reason,
    }
}

#[cfg(test)]
#[path = "channel_intake_tests.rs"]
mod tests;
