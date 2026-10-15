use ironclaw_host_api::{
    action::{NetworkMethod, NetworkPolicy, NetworkScheme, NetworkTarget, NetworkTargetPattern},
    ids::{InvocationId, ProjectId, TenantId, ThreadId, UserId},
    resource::ResourceScope,
};
use ironclaw_network::{
    NetworkPolicyError, NetworkRequest, StaticNetworkPolicyEnforcer, network_target_for_url,
    parse_host_pattern, target_matches_pattern,
};

#[tokio::test]
async fn network_policy_allows_exact_scoped_target_without_executing_io() {
    let scope = sample_scope("tenant-a", "user-a");
    let policy = NetworkPolicy {
        allowed_targets: vec![pattern(
            Some(NetworkScheme::Https),
            "api.example.test",
            Some(443),
        )],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: Vec::new(),
    };
    let enforcer = StaticNetworkPolicyEnforcer::new(policy);

    let permit = enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "api.example.test", Some(443)),
            method: NetworkMethod::Post,
            estimated_bytes: Some(512),
        })
        .await
        .unwrap();

    assert_eq!(permit.scope, scope);
    assert_eq!(permit.target.host, "api.example.test");
    assert_eq!(permit.method, NetworkMethod::Post);
    assert_eq!(permit.estimated_bytes, Some(512));
}

#[tokio::test]
async fn network_policy_supports_one_label_wildcard_hosts_only() {
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
        allowed_targets: vec![pattern(Some(NetworkScheme::Https), "*.example.test", None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: Vec::new(),
    });
    let scope = sample_scope("tenant-a", "user-a");

    enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "api.example.test", None),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap();

    let apex = enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "example.test", None),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap_err();
    assert!(apex.is_target_denied());

    let nested_subdomain = enforcer
        .authorize(NetworkRequest {
            scope,
            target: target(NetworkScheme::Https, "deep.api.example.test", None),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap_err();
    assert!(nested_subdomain.is_target_denied());
}

#[test]
fn network_target_matching_supports_one_label_wildcard_hosts_only() {
    let pattern = pattern(Some(NetworkScheme::Https), "*.example.test", Some(443));
    assert!(target_matches_pattern(
        &target(NetworkScheme::Https, "api.example.test", Some(443)),
        &pattern
    ));
    assert!(!target_matches_pattern(
        &target(NetworkScheme::Https, "deep.api.example.test", Some(443)),
        &pattern
    ));
}

#[tokio::test]
async fn network_policy_supports_any_host_wildcard_with_private_ip_guard() {
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
        allowed_targets: vec![pattern(None, "*", None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: Vec::new(),
    });
    let scope = sample_scope("tenant-a", "user-a");

    enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "api.example.test", Some(443)),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap();

    let private_ip = enforcer
        .authorize(NetworkRequest {
            scope,
            target: target(NetworkScheme::Https, "10.0.0.7", Some(443)),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap_err();

    assert!(private_ip.is_private_target_denied());
}

#[test]
fn network_target_matching_supports_any_host_wildcard() {
    let pattern = pattern(None, "*", None);
    assert!(target_matches_pattern(
        &target(NetworkScheme::Https, "api.example.test", Some(443)),
        &pattern
    ));
    assert!(target_matches_pattern(
        &target(NetworkScheme::Http, "example.org", Some(80)),
        &pattern
    ));
}

#[test]
fn network_target_for_url_normalizes_default_ports() {
    assert_eq!(
        network_target_for_url("https://api.example.test/repos").unwrap(),
        target(NetworkScheme::Https, "api.example.test", Some(443))
    );
    assert!(network_target_for_url("ftp://api.example.test/repos").is_err());
}

#[tokio::test]
async fn network_policy_denies_scheme_host_port_and_egress_mismatches() {
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
        allowed_targets: vec![pattern(
            Some(NetworkScheme::Https),
            "api.example.test",
            Some(443),
        )],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(10),
        private_network_targets: Vec::new(),
    });
    let scope = sample_scope("tenant-a", "user-a");

    let wrong_scheme = enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Http, "api.example.test", Some(443)),
            method: NetworkMethod::Get,
            estimated_bytes: Some(1),
        })
        .await
        .unwrap_err();
    assert!(wrong_scheme.is_target_denied());

    let wrong_host = enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "evil.example.test", Some(443)),
            method: NetworkMethod::Get,
            estimated_bytes: Some(1),
        })
        .await
        .unwrap_err();
    assert!(wrong_host.is_target_denied());

    let wrong_port = enforcer
        .authorize(NetworkRequest {
            scope: scope.clone(),
            target: target(NetworkScheme::Https, "api.example.test", Some(8443)),
            method: NetworkMethod::Get,
            estimated_bytes: Some(1),
        })
        .await
        .unwrap_err();
    assert!(wrong_port.is_target_denied());

    let too_large = enforcer
        .authorize(NetworkRequest {
            scope,
            target: target(NetworkScheme::Https, "api.example.test", Some(443)),
            method: NetworkMethod::Post,
            estimated_bytes: Some(11),
        })
        .await
        .unwrap_err();
    assert!(too_large.is_egress_limit_exceeded());
}

#[tokio::test]
async fn network_policy_denies_non_public_literal_ips_allowed_by_host_contract() {
    for host in ["100.64.0.1", "2001:db8::1"] {
        let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
            allowed_targets: vec![pattern(Some(NetworkScheme::Http), host, None)],
            deny_private_ip_ranges: true,
            max_egress_bytes: Some(1024),
            private_network_targets: Vec::new(),
        });
        let scope = sample_scope("tenant-a", "user-a");

        let err = enforcer
            .authorize(NetworkRequest {
                scope,
                target: target(NetworkScheme::Http, host, None),
                method: NetworkMethod::Get,
                estimated_bytes: Some(0),
            })
            .await
            .unwrap_err();

        assert!(err.is_private_target_denied());
        assert!(!format!("{err}").contains("/tmp"));
    }
}

#[tokio::test]
async fn network_policy_denies_ipv4_mapped_private_ipv6_literals() {
    for host in ["::ffff:127.0.0.1", "::ffff:10.0.0.1"] {
        let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
            allowed_targets: vec![pattern(Some(NetworkScheme::Http), host, None)],
            deny_private_ip_ranges: true,
            max_egress_bytes: Some(1024),
            private_network_targets: Vec::new(),
        });
        let scope = sample_scope("tenant-a", "user-a");

        let err = enforcer
            .authorize(NetworkRequest {
                scope,
                target: target(NetworkScheme::Http, host, None),
                method: NetworkMethod::Get,
                estimated_bytes: Some(0),
            })
            .await
            .unwrap_err();

        assert!(err.is_private_target_denied());
    }

    let public_mapped_host = "::ffff:8.8.8.8";
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
        allowed_targets: vec![pattern(Some(NetworkScheme::Http), public_mapped_host, None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: Vec::new(),
    });
    enforcer
        .authorize(NetworkRequest {
            scope: sample_scope("tenant-a", "user-a"),
            target: target(NetworkScheme::Http, public_mapped_host, None),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn network_policy_requires_egress_estimate_when_limit_is_configured() {
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy {
        allowed_targets: vec![pattern(
            Some(NetworkScheme::Https),
            "api.example.test",
            Some(443),
        )],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(10),
        private_network_targets: Vec::new(),
    });
    let scope = sample_scope("tenant-a", "user-a");

    let err = enforcer
        .authorize(NetworkRequest {
            scope,
            target: target(NetworkScheme::Https, "api.example.test", Some(443)),
            method: NetworkMethod::Get,
            estimated_bytes: None,
        })
        .await
        .unwrap_err();

    assert!(err.is_egress_estimate_required());
}

#[tokio::test]
async fn network_policy_is_fail_closed_without_allowed_targets() {
    let enforcer = StaticNetworkPolicyEnforcer::new(NetworkPolicy::default());
    let scope = sample_scope("tenant-a", "user-a");

    let err = enforcer
        .authorize(NetworkRequest {
            scope,
            target: target(NetworkScheme::Https, "api.example.test", None),
            method: NetworkMethod::Get,
            estimated_bytes: None,
        })
        .await
        .unwrap_err();

    assert!(err.is_target_denied());
}

// --- Private-network exemption (`NetworkPolicy::private_network_targets`) ---
//
// The literal-IP path: the target host is itself an IP address, so the
// decision is made by the static enforcer before any DNS lookup.

/// A policy that allows exactly `host` and exempts exactly `host` from the
/// private-address denial.
fn exempt_literal_policy(host: &str) -> NetworkPolicy {
    NetworkPolicy {
        allowed_targets: vec![pattern(None, host, None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: vec![pattern(None, host, None)],
    }
}

async fn authorize_literal(
    policy: NetworkPolicy,
    scheme: NetworkScheme,
    host: &str,
    port: Option<u16>,
) -> Result<NetworkTarget, NetworkPolicyError> {
    StaticNetworkPolicyEnforcer::new(policy)
        .authorize(NetworkRequest {
            scope: sample_scope("tenant-a", "user-a"),
            target: target(scheme, host, port),
            method: NetworkMethod::Get,
            estimated_bytes: Some(0),
        })
        .await
        .map(|permit| permit.target)
}

#[tokio::test]
async fn private_exemption_allows_exempt_literal_private_addresses() {
    for host in [
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.10",
        "127.0.0.1",
        "::1",
        "fd00::1",
        "100.64.0.1",
        "::ffff:10.0.0.1",
    ] {
        let allowed = authorize_literal(
            exempt_literal_policy(host),
            NetworkScheme::Http,
            host,
            Some(8080),
        )
        .await
        .unwrap_or_else(|error| panic!("exempt private literal {host} should be allowed: {error}"));
        assert_eq!(allowed.host, host);
    }
}

#[tokio::test]
async fn private_exemption_never_allows_always_blocked_literal_addresses() {
    for host in [
        "169.254.169.254",
        "169.254.1.1",
        "fe80::1",
        "224.0.0.1",
        "ff02::1",
        "0.0.0.0",
        "0.1.2.3",
        "::",
        "255.255.255.255",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "2001:db8::1",
        "::ffff:169.254.169.254",
    ] {
        let error = authorize_literal(
            exempt_literal_policy(host),
            NetworkScheme::Http,
            host,
            Some(8080),
        )
        .await
        .expect_err("always-blocked addresses stay denied for exempt targets");
        assert!(
            error.is_private_target_denied(),
            "{host} should be a private-target denial, got {error:?}"
        );
    }
}

#[tokio::test]
async fn private_exemption_does_not_cover_other_literal_targets() {
    // `*` allows every host, and a different host is exempt: a private literal
    // that is not itself exempt is still denied exactly as before.
    let policy = NetworkPolicy {
        allowed_targets: vec![pattern(None, "*", None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: vec![pattern(None, "10.0.0.5", None)],
    };
    for host in ["10.0.0.7", "127.0.0.1", "fd00::2", "100.64.0.9"] {
        let error = authorize_literal(policy.clone(), NetworkScheme::Https, host, None)
            .await
            .expect_err("non-exempt private literal must stay denied");
        assert!(error.is_private_target_denied(), "{host}: {error:?}");
    }
    authorize_literal(policy, NetworkScheme::Https, "10.0.0.5", None)
        .await
        .expect("the exempt literal itself is allowed");
}

#[tokio::test]
async fn private_exemption_is_not_an_allow_grant() {
    // Exempt but not in `allowed_targets`: the exemption only lifts the
    // private-address denial, it never allows a target by itself.
    let policy = NetworkPolicy {
        allowed_targets: vec![pattern(None, "api.example.test", None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: vec![pattern(None, "10.0.0.5", None)],
    };
    let error = authorize_literal(policy, NetworkScheme::Http, "10.0.0.5", None)
        .await
        .expect_err("exemption without an allow entry must be denied");
    assert!(error.is_target_denied(), "{error:?}");
}

#[tokio::test]
async fn private_exemption_with_a_wildcard_entry_is_refused_outright() {
    // A wildcard exemption would turn the narrow opt-in back into "every host
    // may reach the private network". The whole policy is refused with a
    // typed error — even for a public target, and even when another entry
    // is a valid literal — so a misconfiguration fails loudly instead of
    // quietly doing less than its author meant.
    for wildcard in ["*", "*.internal"] {
        for host in ["10.0.0.7", "93.184.216.34"] {
            let policy = NetworkPolicy {
                allowed_targets: vec![pattern(None, "*", None)],
                deny_private_ip_ranges: true,
                max_egress_bytes: Some(1024),
                private_network_targets: vec![
                    pattern(None, "10.0.0.7", None),
                    pattern(None, wildcard, None),
                ],
            };
            let error = authorize_literal(policy, NetworkScheme::Http, host, None)
                .await
                .expect_err("a policy carrying a wildcard exemption must be refused");
            assert!(
                error.is_wildcard_private_network_target(),
                "{wildcard} / {host}: {error:?}"
            );
        }
    }
}

#[tokio::test]
async fn private_exemption_must_match_scheme_and_port_when_given() {
    let policy = NetworkPolicy {
        allowed_targets: vec![pattern(None, "10.0.0.5", None)],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(1024),
        private_network_targets: vec![pattern(Some(NetworkScheme::Http), "10.0.0.5", Some(8080))],
    };

    authorize_literal(policy.clone(), NetworkScheme::Http, "10.0.0.5", Some(8080))
        .await
        .expect("matching scheme and port is exempt");

    for (scheme, port) in [
        (NetworkScheme::Https, Some(8080)),
        (NetworkScheme::Http, Some(9090)),
        (NetworkScheme::Http, None),
    ] {
        let error = authorize_literal(policy.clone(), scheme, "10.0.0.5", port)
            .await
            .expect_err("a scheme or port outside the exemption stays denied");
        assert!(
            error.is_private_target_denied(),
            "{scheme:?}:{port:?}: {error:?}"
        );
    }
}

fn target(scheme: NetworkScheme, host: &str, port: Option<u16>) -> NetworkTarget {
    NetworkTarget {
        scheme,
        host: host.to_string(),
        port,
    }
}

fn pattern(
    scheme: Option<NetworkScheme>,
    host_pattern: &str,
    port: Option<u16>,
) -> NetworkTargetPattern {
    NetworkTargetPattern {
        scheme,
        host_pattern: host_pattern.to_string(),
        port,
    }
}

// `parse_host_pattern` is the chokepoint an untrusted operator-supplied
// hostname string (e.g. the sandbox's `IRONCLAW_SANDBOX_EXTRA_ALLOWED_DOMAINS`)
// must go through before becoming policy. Unlike `NetworkTargetPattern::
// validate_declaration` (shape-only, permits a reviewed `*` for legitimate
// full-access grants), this rejects the bare wildcard outright: a typo in an
// env var was never reviewed, and `host_matches_pattern` treats `*` as
// "match every host".
#[test]
fn parse_host_pattern_accepts_plain_and_wildcard_hostnames() {
    let plain = parse_host_pattern("crates.io").unwrap();
    assert_eq!(plain.host_pattern, "crates.io");
    assert_eq!(plain.scheme, None);
    assert_eq!(plain.port, None);

    let wildcard = parse_host_pattern("*.corp.example.com").unwrap();
    assert_eq!(wildcard.host_pattern, "*.corp.example.com");
}

#[test]
fn parse_host_pattern_trims_surrounding_whitespace() {
    let pattern = parse_host_pattern("  crates.io  ").unwrap();
    assert_eq!(pattern.host_pattern, "crates.io");
}

#[test]
fn parse_host_pattern_rejects_empty_and_bare_wildcard() {
    assert!(
        parse_host_pattern("")
            .unwrap_err()
            .is_invalid_host_pattern()
    );
    assert!(
        parse_host_pattern("   ")
            .unwrap_err()
            .is_invalid_host_pattern()
    );
    assert!(
        parse_host_pattern("*")
            .unwrap_err()
            .is_invalid_host_pattern()
    );
}

// `parse_host_pattern` is documented as the *stricter* chokepoint relative
// to `NetworkTargetPattern::validate_declaration`
// (`crates/contracts/ironclaw_host_api/src/action.rs`), which caps host patterns at
// 253 bytes (the DNS name length limit). A validator billed as strictly
// stricter must not be laxer on any axis, including length — an unbounded
// `parse_host_pattern` would let an operator-supplied env var push an
// arbitrarily large string into `NetworkPolicy::allowed_targets` that
// `validate_declaration` would have rejected.
#[test]
fn parse_host_pattern_rejects_patterns_over_253_bytes() {
    let too_long = format!("{}.example.com", "a".repeat(250));
    assert!(too_long.len() > 253);
    assert!(
        parse_host_pattern(&too_long)
            .unwrap_err()
            .is_invalid_host_pattern(),
        "parse_host_pattern must reject patterns over 253 bytes, matching \
         validate_declaration's length cap"
    );
}

#[test]
fn parse_host_pattern_accepts_pattern_at_exactly_253_bytes() {
    // 253 bytes is the DNS name length limit and validate_declaration's
    // exact cap; parse_host_pattern must not be stricter than that either.
    let label = "a".repeat(249);
    let at_limit = format!("{label}.com");
    assert_eq!(at_limit.len(), 253);
    assert!(parse_host_pattern(&at_limit).is_ok());
}

#[test]
fn parse_host_pattern_rejects_malformed_hostnames() {
    for bad in [
        "not a host!",
        "..leading-dot.com",
        "trailing-dot.com.",
        "double..dot.com",
        "has space.com",
        "embedded\0nul.com",
    ] {
        assert!(
            parse_host_pattern(bad)
                .unwrap_err()
                .is_invalid_host_pattern(),
            "expected {bad:?} to be rejected"
        );
    }
}

fn sample_scope(tenant: &str, user: &str) -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new(tenant).unwrap(),
        user_id: UserId::new(user).unwrap(),
        agent_id: None,
        project_id: Some(ProjectId::new("project-a").unwrap()),
        mission_id: None,
        thread_id: Some(ThreadId::new("thread-a").unwrap()),
        invocation_id: InvocationId::new(),
    }
}
