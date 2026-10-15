//! Network policies the sandbox managed-egress proxy cannot represent
//! exactly. Each one fails closed before a proxy is configured, so the proxy
//! never enforces a policy wider than the one the host authorized.

use ironclaw_host_api::{action::NetworkPolicy, process::RuntimeProcessError};

/// Refuse the policy shapes iron-proxy cannot reproduce:
///
/// - Wildcard host patterns. Proxy globs match the apex and subdomains at any
///   depth, which is strictly wider than
///   [`ironclaw_network::target_matches_pattern`]'s exactly-one-label wildcard.
/// - Private-network exemptions (`NetworkPolicy::private_network_targets`).
///   The proxy has no per-target private-range switch, so honouring one would
///   either drop the exemption or open private ranges for every target.
pub(super) fn reject_unrepresentable_targets(
    policy: &NetworkPolicy,
) -> Result<(), RuntimeProcessError> {
    if policy
        .allowed_targets
        .iter()
        .any(|target| target.host_pattern.starts_with('*'))
    {
        return Err(RuntimeProcessError::ExecutionFailed(
            "sandbox managed egress proxy cannot represent wildcard host patterns exactly \
             (proxy globs also match the apex and deeper subdomains); enumerate exact \
             hostnames instead"
                .to_string(),
        ));
    }
    if !policy.private_network_targets.is_empty() {
        return Err(RuntimeProcessError::ExecutionFailed(
            "sandbox managed egress proxy cannot honour private-network exemptions".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::action::{NetworkPolicy, NetworkTargetPattern};

    fn exact(host: &str) -> NetworkTargetPattern {
        NetworkTargetPattern {
            scheme: None,
            host_pattern: host.to_string(),
            port: None,
        }
    }

    fn policy() -> NetworkPolicy {
        NetworkPolicy {
            allowed_targets: vec![exact("github.com")],
            deny_private_ip_ranges: true,
            ..NetworkPolicy::default()
        }
    }

    /// Driven through the renderer, the caller that turns a policy into proxy
    /// configuration, so the check cannot be bypassed by a second path.
    #[test]
    fn the_renderer_refuses_policies_the_proxy_cannot_represent() {
        let render =
            |policy: &NetworkPolicy| super::super::render_proxy_config(policy, "172.28.10.2");
        render(&policy()).expect("an exact-host policy renders");

        let mut wildcard = policy();
        wildcard.allowed_targets.push(exact("*.github.com"));
        assert!(render(&wildcard).is_err());

        let mut private_exempt = policy();
        private_exempt.private_network_targets = vec![exact("github.com")];
        assert!(render(&private_exempt).is_err());
    }
}
