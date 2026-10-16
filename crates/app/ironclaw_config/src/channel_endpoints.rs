//! `[channel_endpoints]`: operator-bound addresses for extension egress
//! endpoints.
//!
//! An extension manifest may name an egress *endpoint* instead of a literal
//! host when the service it talks to only has a deployment-specific address
//! (a sidecar on a private network, say). The operator binds each one here:
//!
//! ```toml
//! [channel_endpoints.sample-ext.relay]
//! url = "http://relay:8080"
//! allow_private_network = true   # default false
//! allow_insecure_http = true     # default false; http needs both flags
//! ```
//!
//! This file is the only place an endpoint can be bound. Tenant administrator
//! configuration has no field for it, so nothing below the operator can point
//! an extension at an address inside the operator's network.
//!
//! The checks here are shape checks that keep a bad file from booting; the
//! extension host re-validates every entry when composition builds the typed
//! bindings.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config_file::RebornConfigFileError;
use crate::secrets_guard::reject_inline_secret;

/// `[channel_endpoints]`: extension id → endpoint id → binding.
pub type ChannelEndpointsSection = BTreeMap<String, BTreeMap<String, ChannelEndpointEntry>>;

/// One `[channel_endpoints.<extension_id>.<endpoint_id>]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEndpointEntry {
    /// `http(s)://host[:port]` with an empty or `/` path. No userinfo, query,
    /// or fragment.
    pub url: String,
    /// Let this endpoint resolve to a private-network address (RFC 1918,
    /// loopback, unique-local, carrier-grade NAT). Default `false`.
    #[serde(default)]
    pub allow_private_network: bool,
    /// Let this endpoint use plain `http`. Requires `allow_private_network`
    /// too. Default `false`.
    #[serde(default)]
    pub allow_insecure_http: bool,
}

/// Longest endpoint id: one DNS label.
const MAX_ENDPOINT_ID_BYTES: usize = 63;
/// Longest extension id, matching the host's extension-id grammar.
const MAX_EXTENSION_ID_BYTES: usize = 128;
/// The reserved domain logical endpoint addresses live in. An operator URL
/// pointing there would loop the rewrite back onto itself.
const LOGICAL_ENDPOINT_SUFFIX: &str = "channel-endpoint.invalid";

pub(crate) fn validate_channel_endpoints(
    section: &ChannelEndpointsSection,
    attributed_path: &Path,
) -> Result<(), RebornConfigFileError> {
    let path = || attributed_path.display().to_string();
    let invalid = |field: String, reason: &str| RebornConfigFileError::InvalidField {
        path: path(),
        field,
        reason: reason.to_string(),
    };
    let secret = |label: String, value: &str| {
        reject_inline_secret(Cow::Owned(label), value).map_err(|source| {
            RebornConfigFileError::InlineSecret {
                path: path(),
                source,
            }
        })
    };
    for (extension_id, endpoints) in section {
        let extension_label = format!("channel_endpoints.{extension_id}");
        secret(extension_label.clone(), extension_id)?;
        if let Err(reason) = extension_id_shape(extension_id) {
            return Err(invalid(extension_label, reason));
        }
        for (endpoint_id, entry) in endpoints {
            let endpoint_label = format!("{extension_label}.{endpoint_id}");
            secret(endpoint_label.clone(), endpoint_id)?;
            if let Err(reason) = endpoint_id_shape(endpoint_id) {
                return Err(invalid(endpoint_label, reason));
            }
            let url_label = format!("{endpoint_label}.url");
            secret(url_label.clone(), &entry.url)?;
            if let Err(reason) = endpoint_url_shape(entry) {
                return Err(invalid(url_label, reason));
            }
        }
    }
    Ok(())
}

/// The host's extension-id grammar: lowercase ASCII letters, digits, `_`,
/// `-`, and `.`; starts with a letter or digit; no empty or `..` segments.
fn extension_id_shape(value: &str) -> Result<(), &'static str> {
    let first = value.bytes().next().ok_or("must not be empty")?;
    if value.len() > MAX_EXTENSION_ID_BYTES {
        return Err("must be at most 128 bytes");
    }
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err("must start with a lowercase ASCII letter or digit");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("only lowercase ASCII letters, digits, '_', '-', and '.' are allowed");
    }
    if value.split('.').any(str::is_empty) {
        return Err("empty dot segments are not allowed");
    }
    Ok(())
}

/// One DNS label: the endpoint id becomes the first label of its logical host.
fn endpoint_id_shape(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("must not be empty");
    }
    if value.len() > MAX_ENDPOINT_ID_BYTES {
        return Err("must be at most 63 bytes");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("must be one DNS label: lowercase ASCII letters, digits, and '-'");
    }
    if value.starts_with('-') || value.ends_with('-') {
        return Err("must start and end with a letter or digit");
    }
    Ok(())
}

fn endpoint_url_shape(entry: &ChannelEndpointEntry) -> Result<(), &'static str> {
    let url = url::Url::parse(entry.url.trim()).map_err(|_| "must be an absolute http(s) URL")?;
    match url.scheme() {
        "https" => {}
        "http" if entry.allow_insecure_http && entry.allow_private_network => {}
        "http" => {
            return Err(
                "an http URL requires both allow_insecure_http = true and allow_private_network = true",
            );
        }
        _ => return Err("scheme must be http or https"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry userinfo");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or fragment");
    }
    if !matches!(url.path(), "" | "/") {
        return Err("path must be empty or `/`");
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host.is_empty() {
        return Err("must name a host");
    }
    if host.contains('*') {
        return Err("must name a literal host, not a wildcard");
    }
    if host
        .trim_end_matches('.')
        .ends_with(LOGICAL_ENDPOINT_SUFFIX)
    {
        return Err("must not point into the reserved channel-endpoint.invalid domain");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::RebornConfigFile;
    use std::path::Path;

    fn parse(text: &str) -> Result<RebornConfigFile, String> {
        RebornConfigFile::parse_text(text, Path::new("/home/op/config.toml"))
            .map_err(|error| error.to_string())
    }

    #[test]
    fn an_endpoint_table_parses_with_safe_defaults() {
        let config =
            parse("[channel_endpoints.sample-ext.relay]\nurl = \"https://relay.internal:8443\"\n")
                .expect("an https endpoint with defaults parses");
        let entry = &config.channel_endpoints.expect("section present")["sample-ext"]["relay"];
        assert_eq!(entry.url, "https://relay.internal:8443");
        assert!(!entry.allow_private_network);
        assert!(!entry.allow_insecure_http);

        let config = parse(
            "[channel_endpoints.sample-ext.relay]\nurl = \"http://relay:8080\"\n\
             allow_private_network = true\nallow_insecure_http = true\n",
        )
        .expect("http with both opt-ins parses");
        assert!(config.channel_endpoints.is_some());
    }

    #[test]
    fn invalid_endpoint_tables_fail_closed_naming_the_field() {
        let table = "[channel_endpoints.sample-ext.relay]";
        for (body, field, reason) in [
            ("url = \"http://relay:8080\"", "relay.url", "requires both"),
            (
                "url = \"http://relay:8080\"\nallow_insecure_http = true",
                "relay.url",
                "requires both",
            ),
            (
                "url = \"http://relay:8080\"\nallow_private_network = true",
                "relay.url",
                "requires both",
            ),
            ("url = \"ftp://relay\"", "relay.url", "scheme"),
            ("url = \"https://user:pw@relay\"", "relay.url", "userinfo"),
            ("url = \"https://relay/?a=1\"", "relay.url", "query"),
            ("url = \"https://relay/#top\"", "relay.url", "fragment"),
            ("url = \"https://relay/v1\"", "relay.url", "path"),
            ("url = \"https://*.internal\"", "relay.url", "wildcard"),
            (
                "url = \"https://x.channel-endpoint.invalid.\"",
                "relay.url",
                "reserved",
            ),
            ("url = \"relay:8080\"", "relay.url", "scheme"),
            ("url = \"not a url\"", "relay.url", "absolute"),
        ] {
            let error = parse(&format!("{table}\n{body}\n")).expect_err(body);
            assert!(
                error.contains(field) && error.contains(reason),
                "{body}: {error}"
            );
        }
    }

    #[test]
    fn endpoint_and_extension_ids_follow_the_host_grammar() {
        for (table, field) in [
            (
                "[channel_endpoints.Sample.relay]",
                "channel_endpoints.Sample",
            ),
            (
                "[channel_endpoints.\"sample..x\".relay]",
                "channel_endpoints.sample..x",
            ),
            ("[channel_endpoints.sample-ext.re_lay]", "sample-ext.re_lay"),
            ("[channel_endpoints.sample-ext.Relay]", "sample-ext.Relay"),
            ("[channel_endpoints.sample-ext.-relay]", "sample-ext.-relay"),
        ] {
            let error = parse(&format!("{table}\nurl = \"https://relay\"\n")).expect_err(table);
            assert!(error.contains(field), "{table}: {error}");
        }
    }

    #[test]
    fn unknown_keys_and_inline_secrets_are_refused() {
        let error =
            parse("[channel_endpoints.sample-ext.relay]\nurl = \"https://relay\"\ntoken = \"x\"\n")
                .expect_err("unknown key");
        assert!(error.contains("token"), "{error}");

        let error = parse(
            "[channel_endpoints.sample-ext.relay]\n\
             url = \"https://relay/?k=sk-proj-1234567890abcdef1234567890\"\n",
        )
        .expect_err("a secret-shaped value is refused before any shape check echoes it");
        assert!(!error.contains("sk-proj-1234567890abcdef"), "{error}");
        assert!(
            error.contains("channel_endpoints.sample-ext.relay.url"),
            "{error}"
        );
    }
}
