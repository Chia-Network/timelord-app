use super::*;

#[test]
fn timelord_handshake_capability_is_advertised_once() {
    let greeting = timelord_handshake("mainnet");
    assert_eq!(
        greeting
            .capabilities
            .iter()
            .filter(|capability| capability.0 == 6)
            .count(),
        1
    );
    assert!(greeting.capabilities.contains(&(6, "1".to_owned())));
    let other = handshake("mainnet", NodeType::FullNode, 0);
    assert!(
        !other
            .capabilities
            .iter()
            .any(|capability| capability.0 == 6)
    );
}
