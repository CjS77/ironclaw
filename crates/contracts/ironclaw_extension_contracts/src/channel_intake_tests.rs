//! `[channel.intake]` shape rules. Kept out of `channel.rs` so the test
//! cases do not count toward the contracts crate's production size.

use super::*;

/// A webhook intake channel: ingress plus `[intake]`, nothing that would let
/// an outsider's message start a turn, answer, or run a command.
fn intake_channel_toml(extra: &str) -> String {
    format!(
        r#"
id = "messages"
display_name = "Vendor messages"
conversation_model = "continuous"

[intake]

[ingress]
route_suffix = "events"
method = "post"
body_limit_bytes = 1048576

[ingress.verification]
kind = "hmac_sha256"
secret_handle = "vendor_signing_secret"
signature_header = "X-Vendor-Signature"
signature_encoding = "hex"
signed_payload = [{{ body = true }}]
{extra}
"#
    )
}

#[test]
fn an_intake_channel_with_webhook_ingress_is_valid() {
    let channel: ChannelDescriptor =
        toml::from_str(&intake_channel_toml("")).expect("intake channel parses");
    assert!(channel.is_intake());
    channel
        .validate()
        .expect("webhook intake channel validates");
}

#[test]
fn a_channel_without_the_section_is_not_intake_and_keeps_its_serialized_bytes() {
    let channel: ChannelDescriptor = toml::from_str(
        r#"
id = "messages"
display_name = "Vendor messages"
conversation_model = "continuous"
"#,
    )
    .expect("bare channel parses");
    assert!(!channel.is_intake());
    let json = serde_json::to_string(&channel).expect("serializes");
    assert!(
        !json.contains("intake"),
        "absent intake must not change resolved-record bytes: {json}"
    );
}

#[test]
fn intake_requires_webhook_ingress() {
    let channel: ChannelDescriptor = toml::from_str(
        r#"
id = "messages"
display_name = "Vendor messages"
conversation_model = "continuous"

[intake]
"#,
    )
    .expect("parses");
    assert_eq!(
        channel.validate(),
        Err(ChannelDescriptorError::IntakeRequiresWebhookIngress)
    );

    let mut session: ChannelDescriptor = toml::from_str(&intake_channel_toml("")).expect("parses");
    if let Some(ingress) = session.ingress.as_mut() {
        ingress.route_suffix = None;
        ingress.verification = IngressVerificationRecipe::AuthenticatedSession;
    }
    assert_eq!(
        session.validate(),
        Err(ChannelDescriptorError::IntakeRequiresWebhookIngress)
    );
}

#[test]
fn intake_forbids_commands_connection_reply_and_delivery() {
    let cases: [(&str, &str); 4] = [
        ("commands", "commands = [\"stop\"]"),
        (
            "connection",
            r#"
[connection]
provider = "vendor"
strategy = "web_generated_code"
instructions = "Send the code."
submit_label = "Connect"
error_message = "Failed."
connection_success_message = "Connected."
[connection.notices]
connect_required = "Connect first."
paired = "Paired."
already_paired_same_user = "Already paired."
already_bound_to_other_user = "Bound elsewhere."
expired_or_unknown = "Expired."
"#,
        ),
        ("reply", "[reply]\ntransport = \"message\""),
        ("delivery", "[delivery]\ntransport = \"message\""),
    ];
    for (section, extra) in cases {
        // Top-level keys must precede the tables in TOML.
        let toml_text = if section == "commands" {
            intake_channel_toml("").replacen(
                "conversation_model = \"continuous\"",
                &format!("conversation_model = \"continuous\"\n{extra}"),
                1,
            )
        } else {
            intake_channel_toml(extra)
        };
        let channel: ChannelDescriptor = toml::from_str(&toml_text)
            .unwrap_or_else(|error| panic!("{section} case parses: {error}"));
        assert_eq!(
            channel.validate(),
            Err(ChannelDescriptorError::IntakeForbidsSection { section }),
            "intake must refuse `{section}`"
        );
    }
}

#[test]
fn intake_rejects_unknown_fields() {
    let result: Result<ChannelDescriptor, _> =
        toml::from_str(&intake_channel_toml("").replace("[intake]", "[intake]\nmode = \"x\""));
    assert!(result.is_err(), "[channel.intake] is a closed section");
}
