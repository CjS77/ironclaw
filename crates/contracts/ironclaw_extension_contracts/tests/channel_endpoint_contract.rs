//! `ChannelEgressEndpointId` grammar, and that the descriptor accepts an
//! endpoint target and keeps a literal target's persisted form, through the
//! same TOML deserialization manifest parsing uses. The host/endpoint shape
//! rules are the manifest parser's (`ironclaw_extension_registry`).

use ironclaw_extension_contracts::channel_endpoint::ChannelEgressEndpointId;
#[test]
fn endpoint_ids_are_single_dns_labels() {
    for valid in ["relay", "a", "side-car-2", &"x".repeat(63)] {
        assert!(ChannelEgressEndpointId::new(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "Relay",
        "re_lay",
        "re.lay",
        "-relay",
        "relay-",
        "re lay",
        &"x".repeat(64),
    ] {
        assert!(ChannelEgressEndpointId::new(invalid).is_err(), "{invalid}");
    }
    assert!(serde_json::from_str::<ChannelEgressEndpointId>("\"Bad_Id\"").is_err());
    let parsed: ChannelEgressEndpointId = serde_json::from_str("\"relay\"").unwrap();
    assert_eq!(serde_json::to_string(&parsed).unwrap(), "\"relay\"");
}

/// A minimal delivery-only channel whose one egress target is `target`.
fn channel_with_target(target: &str) -> ironclaw_extension_contracts::channel::ChannelDescriptor {
    toml::from_str(&format!(
        "id = \"messages\"\ndisplay_name = \"Messages\"\nconversation_model = \"continuous\"\n\n\
         [delivery]\ntransport = \"message\"\n\n[[egress]]\n{target}\nmethods = [\"post\"]\n"
    ))
    .expect("channel descriptor deserializes")
}

#[test]
fn an_endpoint_target_needs_no_literal_host() {
    use ironclaw_extension_contracts::channel::ChannelDescriptorError;

    let endpoint = channel_with_target("endpoint = \"relay\"");
    endpoint.validate().expect("an endpoint target validates");
    let target = &endpoint.egress[0];
    assert_eq!(target.label(), "endpoint:relay");
    assert!(target.host.is_empty());

    // A target with neither a host nor an endpoint is still an empty host.
    assert!(matches!(
        channel_with_target("scheme = \"https\"").validate(),
        Err(ChannelDescriptorError::WildcardOrEmptyEgressHost { .. })
    ));
    // A malformed endpoint id fails at deserialization, before validation.
    assert!(
        toml::from_str::<ironclaw_extension_contracts::channel::ChannelDescriptor>(
            "id = \"m\"\ndisplay_name = \"M\"\nconversation_model = \"continuous\"\n\
             [[egress]]\nendpoint = \"Relay_1\"\nmethods = [\"post\"]\n"
        )
        .is_err()
    );
}

#[test]
fn a_literal_target_serializes_without_endpoint_vocabulary() {
    let literal = channel_with_target("host = \"vendor.example\"");
    let json = serde_json::to_value(&literal.egress[0]).unwrap();
    assert_eq!(json["host"], "vendor.example");
    assert!(
        json.get("endpoint").is_none(),
        "a literal target's persisted form must not change"
    );
    let endpoint = channel_with_target("endpoint = \"relay\"");
    let json = serde_json::to_value(&endpoint.egress[0]).unwrap();
    assert!(json.get("host").is_none());
    assert_eq!(json["endpoint"], "relay");
}
