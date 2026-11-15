// RATE_LIMITS_V3 constants pinned entry by entry. These are protocol constants: every entry is
// copied, never defaulted, or the two ends of a link disagree on the window.

use dg_xch_core::protocols::ProtocolMessageTypes;
use dg_xch_core::protocols::rate_limits_v3::{
    MAX_CONFIGURE_RATE_LIMITS_ENTRIES, RlSettingsV3, configure_message, peer_supports_v3,
    settings_from_configure, v3_setting,
};
use dg_xch_core::protocols::shared::{Capability, ConfigureWindowSizes};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

use ProtocolMessageTypes as P;

/// The 13 request types at window_size = 2.
const WINDOW_2: [P; 13] = [
    P::RequestBlocks,
    P::RequestBlock,
    P::RequestBlockHeader,
    P::RequestBlockHeaders,
    P::RequestHeaderBlocks,
    P::RegisterInterestInPuzzleHash,
    P::RegisterInterestInCoin,
    P::RequestPuzzleState,
    P::RequestCoinState,
    P::RequestAdditions,
    P::RequestRemovals,
    P::RequestProofOfWeight,
    P::RequestPuzzleSolution,
];

/// The 23 response/reject types at window_size = None (unlimited).
const UNLIMITED: [P; 23] = [
    P::RespondBlocks,
    P::RejectBlocks,
    P::RespondBlock,
    P::RejectBlock,
    P::RespondBlockHeader,
    P::RejectHeaderRequest,
    P::RespondBlockHeaders,
    P::RejectBlockHeaders,
    P::RespondHeaderBlocks,
    P::RejectHeaderBlocks,
    P::RespondToPhUpdate,
    P::RespondToCoinUpdate,
    P::RespondPuzzleState,
    P::RejectPuzzleState,
    P::RespondCoinState,
    P::RejectCoinState,
    P::RespondAdditions,
    P::RejectAdditionsRequest,
    P::RespondRemovals,
    P::RejectRemovalsRequest,
    P::RespondProofOfWeight,
    P::RespondPuzzleSolution,
    P::RejectPuzzleSolution,
];

#[test]
fn table_matches_chia_exactly() {
    for t in WINDOW_2 {
        assert_eq!(
            v3_setting(t),
            Some(RlSettingsV3 {
                window_size: Some(2)
            }),
            "{t:?} must carry chia's window_size=2"
        );
    }
    for t in UNLIMITED {
        assert_eq!(
            v3_setting(t),
            Some(RlSettingsV3 { window_size: None }),
            "{t:?} must be unlimited (response/reject class)"
        );
    }
    // Everything else is OUTSIDE the v3 table (stays time-based) — count the members.
    let mut in_table = 0;
    for code in 0..=u8::MAX {
        let t = ProtocolMessageTypes::from(code);
        if t != ProtocolMessageTypes::Unknown && v3_setting(t).is_some() {
            in_table += 1;
        }
    }
    assert_eq!(in_table, 36, "chia's table has exactly 36 entries");
    assert!(v3_setting(P::NewPeak).is_none());
    assert!(v3_setting(P::NewTransaction).is_none());
    assert!(v3_setting(P::Handshake).is_none());
    assert!(v3_setting(P::ConfigureWindowSizes).is_none());
    assert_eq!(MAX_CONFIGURE_RATE_LIMITS_ENTRIES, 256);
    assert_eq!(Capability::RateLimitsV3 as u16, 7);
    assert_eq!(Capability::HardFork2 as u16, 6);
    assert_eq!(Capability::MempoolUpdates as u16, 5);
}

#[test]
fn configure_message_round_trips_and_parses_to_the_table() {
    let v = ChiaProtocolVersion::default();
    let msg = configure_message();
    assert_eq!(msg.settings.len(), 36);
    let bytes = msg.to_bytes(v).unwrap();
    let back = ConfigureWindowSizes::from_bytes(&mut Cursor::new(bytes.as_slice()), v).unwrap();
    assert_eq!(back, msg, "wire round-trip");

    let parsed = settings_from_configure(&back).expect("our own table validates");
    assert_eq!(parsed.len(), 36);
    for t in WINDOW_2 {
        assert_eq!(parsed[&t].window_size, Some(2));
    }
    for t in UNLIMITED {
        assert_eq!(parsed[&t].window_size, None);
    }
}

// Validation semantics: empty invalid, oversize invalid, unknown skipped, unlimited-override
// invalid, 0 = unlimited.
#[test]
fn configure_validation_matches_chia() {
    // Empty → INVALID_HANDSHAKE.
    assert!(settings_from_configure(&ConfigureWindowSizes { settings: vec![] }).is_err());

    // > 256 entries → INVALID_HANDSHAKE.
    let oversized = ConfigureWindowSizes {
        settings: (0..257u16)
            .map(|i| (u8::try_from(i % 256).unwrap(), 1u16))
            .collect(),
    };
    assert!(settings_from_configure(&oversized).is_err());

    // An unknown message-type code is silently skipped.
    let with_unknown = ConfigureWindowSizes {
        settings: vec![(254u8, 5u16), (P::RequestBlocks as u8, 4u16)],
    };
    let parsed = settings_from_configure(&with_unknown).expect("unknown codes are skipped");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[&P::RequestBlocks].window_size, Some(4));

    // A peer must not bound one of OUR unlimited (response) types.
    let override_unlimited = ConfigureWindowSizes {
        settings: vec![(P::RespondBlocks as u8, 1u16)],
    };
    assert!(
        settings_from_configure(&override_unlimited).is_err(),
        "bounding our unlimited RespondBlocks is INVALID_HANDSHAKE"
    );
    // ...but declaring it unlimited (0) is fine.
    let zero_ok = ConfigureWindowSizes {
        settings: vec![(P::RespondBlocks as u8, 0u16)],
    };
    assert_eq!(
        settings_from_configure(&zero_ok).unwrap()[&P::RespondBlocks].window_size,
        None
    );
}

#[test]
fn capability_probe_reads_state_one_only() {
    assert!(peer_supports_v3(&vec![(7u16, "1".to_string())]));
    assert!(!peer_supports_v3(&vec![(7u16, "0".to_string())]));
    assert!(!peer_supports_v3(&vec![(3u16, "1".to_string())]));
    assert!(!peer_supports_v3(&Vec::new()));
}
