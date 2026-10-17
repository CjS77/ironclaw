use super::*;

#[test]
fn intake_read_tools_are_discovered_with_messaging() {
    for id in ["builtin.intake.list", "builtin.intake.read"] {
        assert_eq!(
            discovery_namespace(&CapabilityId::new(id).expect("capability id")),
            DiscoveryNamespace::Messaging,
            "{id}"
        );
    }
}
