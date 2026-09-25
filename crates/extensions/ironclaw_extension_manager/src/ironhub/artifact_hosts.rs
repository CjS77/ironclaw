//! The hardcoded IronHub download whitelist.
//!
//! Two fixed lists, deliberately not configurable at runtime:
//!
//! - [`IRONHUB_URL_PREFIXES`]: the URL every IronHub request *starts* at (the
//!   catalog manifest, a private manifest, every artifact) must begin with one
//!   of these. `IRONHUB_MANIFEST_URL` is checked against it at startup.
//! - [`IRONHUB_HOSTS`]: the network policy granted to IronHub downloads. The
//!   host egress re-authorizes every redirect hop against that policy, so a
//!   hop may land on these exact hosts only — no wildcards.

use ironclaw_host_api::action::{NetworkPolicy, NetworkScheme, NetworkTargetPattern};

/// `(host, path prefix)` pairs a starting URL must match: `https`, no user
/// information, the default port, this exact host, and a path beginning with
/// the prefix. Comparison is case-sensitive.
const IRONHUB_URL_PREFIXES: &[(&str, &str)] = &[
    // Upstream IronHub: the signed catalog and its artifact proxy.
    ("hub.ironclaw.com", "/"),
    // The Naomi catalog, published as release assets of one repository.
    ("github.com", "/CjS77/naomi-addons/releases/download/"),
];

/// Exact hosts an IronHub download or any of its redirect hops may reach.
/// GitHub answers a release-asset download with a redirect to one of the two
/// `githubusercontent.com` hosts.
const IRONHUB_HOSTS: &[&str] = &[
    "hub.ironclaw.com",
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// Why a URL is not on the whitelist, worded without echoing the URL (a
/// private manifest URL carries an access token).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhitelistRejection {
    Host,
    Path,
}

pub(crate) fn check_download_url(url: &url::Url) -> Result<(), WhitelistRejection> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(WhitelistRejection::Host);
    }
    let host = url.host_str().ok_or(WhitelistRejection::Host)?;
    let mut prefixes = IRONHUB_URL_PREFIXES
        .iter()
        .filter(|(allowed, _)| *allowed == host)
        .peekable();
    if prefixes.peek().is_none() {
        return Err(WhitelistRejection::Host);
    }
    let path = url.path();
    // `url` already resolves `.`/`..` segments (including `%2e` forms) and
    // turns `\` into `/`; an encoded separator is the one way left for a path
    // to leave its prefix after the server decodes it.
    let lowered = path.to_ascii_lowercase();
    if lowered.contains("%2f") || lowered.contains("%5c") {
        return Err(WhitelistRejection::Path);
    }
    if prefixes.any(|(_, prefix)| path.starts_with(prefix)) {
        Ok(())
    } else {
        Err(WhitelistRejection::Path)
    }
}

pub fn artifact_network_policy() -> NetworkPolicy {
    NetworkPolicy {
        allowed_targets: IRONHUB_HOSTS
            .iter()
            .map(|host| NetworkTargetPattern {
                scheme: Some(NetworkScheme::Https),
                host_pattern: (*host).to_string(),
                port: None,
            })
            .collect(),
        deny_private_ip_ranges: true,
        max_egress_bytes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(value: &str) -> Result<(), WhitelistRejection> {
        check_download_url(&url::Url::parse(value).expect("test URL parses"))
    }

    #[test]
    fn whitelist_accepts_upstream_hub_and_the_naomi_release_prefix_only() {
        assert_eq!(
            check("https://hub.ironclaw.com/api/catalog/manifest.json"),
            Ok(())
        );
        assert_eq!(check("https://HUB.IRONCLAW.COM/api/catalog/x"), Ok(()));
        assert_eq!(
            check("https://github.com/CjS77/naomi-addons/releases/download/v1/manifest.json"),
            Ok(())
        );

        for (value, rejection) in [
            (
                "https://github.com/attacker/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons-evil/releases/download/v1/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons/archive/v1.zip",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/cjs77/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons/releases/download/../../../attacker/x/releases/download/v1/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons/releases/download/%2e%2e/%2e%2e/%2e%2e/attacker/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons/releases/download/..%2F..%2Fattacker/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://github.com/CjS77/naomi-addons/releases/download/..%5c..%5cattacker/m.json",
                WhitelistRejection::Path,
            ),
            (
                "https://release-assets.githubusercontent.com/github-production-release-asset/1/m.json",
                WhitelistRejection::Host,
            ),
            (
                "https://raw.githubusercontent.com/CjS77/naomi-addons/main/m.json",
                WhitelistRejection::Host,
            ),
            (
                "http://github.com/CjS77/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Host,
            ),
            (
                "https://github.com:8443/CjS77/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Host,
            ),
            (
                "https://user@github.com/CjS77/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Host,
            ),
            (
                "https://github.com.evil.example/CjS77/naomi-addons/releases/download/v1/m.json",
                WhitelistRejection::Host,
            ),
            (
                "https://hub.ironclaw.com.evil.example/api/catalog/manifest.json",
                WhitelistRejection::Host,
            ),
        ] {
            assert_eq!(check(value), Err(rejection), "{value}");
        }
    }

    #[test]
    fn artifact_policy_lists_exact_hosts_without_wildcards() {
        let policy = artifact_network_policy();
        assert!(policy.deny_private_ip_ranges);
        let hosts = policy
            .allowed_targets
            .iter()
            .map(|target| {
                assert_eq!(target.scheme, Some(NetworkScheme::Https));
                assert_eq!(target.port, None);
                target.host_pattern.as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(hosts, IRONHUB_HOSTS);
        assert!(hosts.iter().all(|host| !host.contains('*')));
        // Every starting-URL host is also reachable, or a whitelisted URL could
        // never be fetched.
        for (host, _) in IRONHUB_URL_PREFIXES {
            assert!(hosts.contains(host), "{host} missing from the policy");
        }
    }
}
