//! Resolve `[channel_endpoints]` from `config.toml` into the typed endpoint
//! bindings the extension host enforces.
//!
//! `ironclaw_config` already checked the shape at parse time; this is where
//! the strings become typed ids and a validated origin, and the two boolean
//! opt-ins become explicit postures. Any entry the host refuses stops startup
//! rather than leaving an endpoint silently unbound.

use anyhow::Context;
use ironclaw_config::ChannelEndpointsSection;
use ironclaw_extension_contracts::channel_endpoint::ChannelEgressEndpointId;
use ironclaw_extension_host::channel_endpoints::{
    InsecureHttpPosture, OperatorChannelEndpoint, OperatorChannelEndpoints, PrivateNetworkPosture,
};
use ironclaw_host_api::ids::ExtensionId;

pub(super) fn operator_channel_endpoints(
    config_file: Option<&ironclaw_config::RebornConfigFile>,
) -> anyhow::Result<OperatorChannelEndpoints> {
    match config_file.and_then(|file| file.channel_endpoints.as_ref()) {
        Some(section) => resolve(section),
        None => Ok(OperatorChannelEndpoints::new()),
    }
}

fn resolve(section: &ChannelEndpointsSection) -> anyhow::Result<OperatorChannelEndpoints> {
    let mut endpoints = OperatorChannelEndpoints::new();
    for (extension_id, entries) in section {
        let typed_extension = ExtensionId::new(extension_id.as_str())
            .with_context(|| format!("[channel_endpoints.{extension_id}] extension id"))?;
        for (endpoint_id, entry) in entries {
            let field = format!("[channel_endpoints.{extension_id}.{endpoint_id}]");
            let typed_endpoint = ChannelEgressEndpointId::new(endpoint_id.as_str())
                .with_context(|| format!("{field} endpoint id"))?;
            let private_network = if entry.allow_private_network {
                PrivateNetworkPosture::Allowed
            } else {
                PrivateNetworkPosture::Denied
            };
            let insecure_http = if entry.allow_insecure_http {
                InsecureHttpPosture::Allowed
            } else {
                InsecureHttpPosture::Denied
            };
            let endpoint =
                OperatorChannelEndpoint::parse(&entry.url, private_network, insecure_http)
                    .with_context(|| format!("{field} url"))?;
            endpoints
                .insert(typed_extension.clone(), typed_endpoint, endpoint)
                .with_context(|| field.clone())?;
        }
    }
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use ironclaw_host_api::action::NetworkScheme;

    fn config(text: &str) -> ironclaw_config::RebornConfigFile {
        ironclaw_config::RebornConfigFile::parse_text(text, Path::new("config.toml"))
            .expect("config parses")
    }

    #[test]
    fn no_section_binds_nothing() {
        assert!(operator_channel_endpoints(None).unwrap().is_empty());
        assert!(
            operator_channel_endpoints(Some(&config("")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn entries_map_to_typed_bindings_with_their_postures() {
        let endpoints = operator_channel_endpoints(Some(&config(
            "[channel_endpoints.acme-inbox.relay]\nurl = \"http://relay:8080\"\n\
             allow_private_network = true\nallow_insecure_http = true\n\n\
             [channel_endpoints.acme-inbox.archive]\nurl = \"https://archive.example\"\n",
        )))
        .expect("valid entries resolve");
        assert_eq!(endpoints.len(), 2);

        let relay = endpoints
            .get(
                "acme-inbox",
                &ChannelEgressEndpointId::new("relay").unwrap(),
            )
            .expect("relay is bound");
        assert_eq!(relay.scheme(), NetworkScheme::Http);
        assert_eq!(relay.host(), "relay");
        assert_eq!(relay.port(), 8080);
        assert_eq!(relay.private_network(), PrivateNetworkPosture::Allowed);

        let archive = endpoints
            .get(
                "acme-inbox",
                &ChannelEgressEndpointId::new("archive").unwrap(),
            )
            .expect("archive is bound");
        assert_eq!(archive.scheme(), NetworkScheme::Https);
        assert_eq!(
            archive.private_network(),
            PrivateNetworkPosture::Denied,
            "the private-network opt-in defaults off"
        );
    }
}
