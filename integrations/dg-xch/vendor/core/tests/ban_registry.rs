use dg_xch_core::protocols::ban::{BanCause, BanRegistry};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

#[test]
fn ban_cause_durations_match_chia() {
    assert_eq!(BanCause::RateLimit.ban_seconds(), 300);
    assert_eq!(BanCause::ConsensusError.ban_seconds(), 600);
    assert_eq!(BanCause::InvalidProtocol.ban_seconds(), 10);
    assert_eq!(BanCause::ApiException.ban_seconds(), 10);
    assert_eq!(BanCause::InternalProtocolError.ban_seconds(), 10);
}

// Test 5: the ban keys on the remote host; a different host is unaffected.
#[test]
fn bans_key_on_host_not_globally() {
    let reg = BanRegistry::default();
    reg.ban(ip(203, 0, 113, 7), BanCause::RateLimit);
    assert!(
        reg.is_banned(&ip(203, 0, 113, 7)),
        "the banned host is refused"
    );
    assert!(
        !reg.is_banned(&ip(203, 0, 113, 8)),
        "a different host is NOT banned — the ban is host-scoped, not a global switch"
    );
}

// Test 2: after the ban expires, the host is allowed again (and the entry is pruned).
#[test]
fn ban_expires_and_is_pruned() {
    let reg = BanRegistry::default();
    let host = ip(198, 51, 100, 4);
    reg.ban_for(host, Duration::from_millis(40));
    assert!(reg.is_banned(&host), "banned within the window");
    std::thread::sleep(Duration::from_millis(80));
    assert!(!reg.is_banned(&host), "the ban lifted after expiry");
    assert_eq!(reg.len(), 0, "the expired entry was pruned");
}

#[test]
fn reban_keeps_the_longer_expiry() {
    let reg = BanRegistry::default();
    let host = ip(198, 51, 100, 9);
    reg.ban_for(host, Duration::from_secs(600));
    reg.ban_for(host, Duration::from_millis(20)); // shorter — must be ignored
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        reg.is_banned(&host),
        "the longer 600s ban stands; the shorter re-ban did not shorten it"
    );
}

// Test 4: the registry is bounded (never exceeds its cap) and prunes expired entries.
#[test]
fn registry_is_bounded_and_prunes() {
    let cap = 16;
    let reg = BanRegistry::new(cap);
    // Insert well over the cap of long-lived bans.
    for i in 0..(cap as u32 + 50) {
        let o = i.to_be_bytes();
        reg.ban_for(ip(10, o[1], o[2], o[3]), Duration::from_secs(600));
    }
    assert!(
        reg.len() <= cap,
        "the registry never exceeds its cap ({} <= {cap})",
        reg.len()
    );

    // A batch of already-expiring bans is pruned by the next probe.
    let short = BanRegistry::new(cap);
    for i in 0..10u32 {
        let o = i.to_be_bytes();
        short.ban_for(ip(172, o[1], o[2], o[3]), Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(short.len(), 0, "expired bans are pruned");
}

#[test]
fn unban_and_clear_reset_state() {
    let reg = BanRegistry::default();
    let host = ip(192, 0, 2, 5);
    reg.ban(host, BanCause::ConsensusError);
    assert!(reg.unban(&host), "unban removes the entry");
    assert!(!reg.is_banned(&host));
    reg.ban(host, BanCause::ConsensusError);
    reg.clear();
    assert!(reg.is_empty(), "clear drops all bans");
}
