// Decode-time list limits, the CPU half.
//
// `dg_xch_serialize::parse_vec_limited` parses the first `min(count, max_items)` elements, then
// skips the remaining FIXED-SIZE elements in O(1) with a cursor seek, so an inflated list claim
// cannot buy unbounded parse CPU. These tests pin the mechanics on the four wallet request types
// that carry limits (register_for_ph_updates / register_for_coin_updates / request_puzzle_state /
// request_coin_state); the over-the-wire handler behavior is pinned in
// `p2p/tests/t049_list_limited_decode.rs`, and the MEMORY half (no pre-allocation from the
// untrusted count) in `streamable_alloc_bomb.rs`.

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::protocols::wallet::{
    CoinStateFilters, RegisterForCoinUpdates, RegisterForPhUpdates, RequestCoinState,
    RequestPuzzleState,
};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

fn version() -> ChiaProtocolVersion {
    ChiaProtocolVersion::default()
}

fn ids(n: usize) -> Vec<Bytes32> {
    (0..n)
        .map(|i| {
            let mut b = [0u8; 32];
            b[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            Bytes32::from(b)
        })
        .collect()
}

// The skip must land the cursor EXACTLY past the tail: the fields AFTER the truncated list
// decode to their true values on every request shape. This is the load-bearing property — an
// off-by-one stride would silently corrupt every post-list field.
#[test]
fn truncated_decode_keeps_head_and_lands_exactly_on_the_next_field() {
    let v = version();
    let sent = ids(100);

    let msg = RegisterForPhUpdates {
        puzzle_hashes: sent.clone(),
        min_height: 0xDEAD_BEEF,
    }
    .to_bytes(v)
    .unwrap();
    let got =
        RegisterForPhUpdates::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, 7).unwrap();
    assert_eq!(got.puzzle_hashes, sent[..7], "head kept, in order");
    assert_eq!(
        got.min_height, 0xDEAD_BEEF,
        "post-list field decodes exactly"
    );

    let msg = RegisterForCoinUpdates {
        coin_ids: sent.clone(),
        min_height: 42,
    }
    .to_bytes(v)
    .unwrap();
    let got =
        RegisterForCoinUpdates::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, 1).unwrap();
    assert_eq!(got.coin_ids, sent[..1]);
    assert_eq!(got.min_height, 42);

    let msg = RequestPuzzleState {
        puzzle_hashes: sent.clone(),
        previous_height: Some(123_456),
        header_hash: Bytes32::from([0xAB; 32]),
        filters: CoinStateFilters {
            include_spent: true,
            include_unspent: false,
            include_hinted: true,
            min_amount: 999,
        },
        subscribe_when_finished: true,
    }
    .to_bytes(v)
    .unwrap();
    let got =
        RequestPuzzleState::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, 3).unwrap();
    assert_eq!(got.puzzle_hashes, sent[..3]);
    assert_eq!(got.previous_height, Some(123_456));
    assert_eq!(got.header_hash, Bytes32::from([0xAB; 32]));
    assert_eq!(got.filters.min_amount, 999);
    assert!(got.subscribe_when_finished);

    let msg = RequestCoinState {
        coin_ids: sent.clone(),
        previous_height: None,
        header_hash: Bytes32::from([0xCD; 32]),
        subscribe: false,
    }
    .to_bytes(v)
    .unwrap();
    let got =
        RequestCoinState::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, 50).unwrap();
    assert_eq!(got.coin_ids, sent[..50]);
    assert_eq!(got.previous_height, None);
    assert_eq!(got.header_hash, Bytes32::from([0xCD; 32]));
    assert!(!got.subscribe);
}

// At or under the limit the limited decode is byte-equivalent to the plain decode — a conforming
// request is untouched: `items_to_parse = min(list_size, max_items)`.
#[test]
fn under_and_at_limit_decode_matches_unlimited() {
    let v = version();
    for n in [0usize, 1, 63, 64] {
        let msg = RegisterForCoinUpdates {
            coin_ids: ids(n),
            min_height: 5,
        }
        .to_bytes(v)
        .unwrap();
        let plain =
            RegisterForCoinUpdates::from_bytes(&mut Cursor::new(msg.as_slice()), v).unwrap();
        let limited =
            RegisterForCoinUpdates::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, 64)
                .unwrap();
        assert_eq!(limited, plain, "n={n}: limited == plain at/under the cap");
    }
}

// A claimed count that overstates the bytes actually present (a pure length-claim bomb): once
// the claim is past the limit the skip runs off the buffer end — a seek past the end succeeds
// and the NEXT field's read then fails at EOF. Both decoders reject the message;
// the limited one must do so WITHOUT attempting to parse the phantom tail (the seek is clamped
// to the buffer end, an O(1) arithmetic step regardless of the claimed count).
#[test]
fn overstated_claim_errors_without_parsing_the_phantom_tail() {
    let v = version();
    // Hand-build: claimed count u32::MAX (a ~137 GiB phantom tail), 4 real elements, min_height.
    let mut raw: Vec<u8> = Vec::new();
    raw.extend(u32::MAX.to_be_bytes());
    for id in ids(4) {
        raw.extend(id.to_bytes(v).unwrap());
    }
    raw.extend(77u32.to_be_bytes());

    let plain = RegisterForPhUpdates::from_bytes(&mut Cursor::new(raw.as_slice()), v);
    assert!(plain.is_err(), "the plain decoder rejects the short list");

    // The limited decode must return promptly (clamped O(1) skip — parsing u32::MAX phantom
    // elements would spin effectively forever) and error on the post-list field.
    let limited = RegisterForPhUpdates::from_bytes_limited(&mut Cursor::new(raw.as_slice()), v, 4);
    assert!(
        limited.is_err(),
        "the field after the over-claimed list fails at EOF"
    );
}

// The motivating DoS shape, bounded: a WELL-FORMED message with far more real elements than the
// cap decodes with only `cap` elements materialized — and the phantom-tail variant above proves
// the tail is skipped, not parsed. 200k elements keeps the test fast while being the real
// untrusted max_subscriptions value.
#[test]
fn over_cap_bulk_decode_materializes_only_the_cap() {
    let v = version();
    let n = 200_017usize;
    let cap = 200_000u32;
    let msg = RequestCoinState {
        coin_ids: ids(n),
        previous_height: Some(1),
        header_hash: Bytes32::from([1; 32]),
        subscribe: true,
    }
    .to_bytes(v)
    .unwrap();
    let got = RequestCoinState::from_bytes_limited(&mut Cursor::new(msg.as_slice()), v, cap)
        .expect("over-cap decode succeeds truncated");
    assert_eq!(got.coin_ids.len(), cap as usize);
    assert_eq!(
        got.previous_height,
        Some(1),
        "post-skip fields land exactly"
    );
    assert!(got.subscribe);
}
