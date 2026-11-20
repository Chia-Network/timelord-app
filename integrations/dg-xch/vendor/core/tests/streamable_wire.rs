//! Wire-level `ChiaSerialize` coverage. The streamable byte format is the network's
//! canonical encoding; these tests assert byte-exact framing for the primitives
//! `ChiaSerialize` builds every wire type out of: `bool`, fixed-width big-endian integers,
//! length-prefixed `String`/byte vectors, `Option` (1-byte presence tag), `Vec` (u32-BE
//! count) and tuples.

use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

const V: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

fn decode<T: ChiaSerialize>(bytes: &[u8]) -> Result<T, std::io::Error> {
    let mut cur = Cursor::new(bytes);
    T::from_bytes(&mut cur, V)
}

// --- bool -------------------------------------------------------------------

#[test]
fn parse_bool_matches_chia() {
    assert!(!decode::<bool>(b"\x00").unwrap());
    assert!(decode::<bool>(b"\x01").unwrap());
    // EOF on empty buffer.
    assert!(decode::<bool>(b"").is_err());
    // A bool byte must be exactly 0 or 1.
    assert!(decode::<bool>(b"\xff").is_err());
    assert!(decode::<bool>(b"\x02").is_err());
}

#[test]
fn bool_round_trips() {
    for b in [false, true] {
        assert_eq!(decode::<bool>(&b.to_bytes(V).unwrap()).unwrap(), b);
    }
}

// --- uint32 -----------------------------------------------------------------

#[test]
fn parse_uint32_is_big_endian_and_length_checked() {
    assert_eq!(decode::<u32>(b"\x00\x00\x00\x00").unwrap(), 0);
    assert_eq!(decode::<u32>(b"\x00\x00\x00\x01").unwrap(), 1);
    assert_eq!(decode::<u32>(b"\x01\x00\x00\x00").unwrap(), 16_777_216);
    assert_eq!(decode::<u32>(b"\xff\xff\xff\xff").unwrap(), 4_294_967_295);
    // A u32 needs exactly four bytes — every short buffer is rejected.
    for short in [&b""[..], b"\x00", b"\x00\x00", b"\x00\x00\x00"] {
        assert!(
            decode::<u32>(short).is_err(),
            "short u32 buffer must reject"
        );
    }
}

#[test]
fn uint32_write_then_read_round_trips() {
    for v in [0u32, 1, 4_294_967_295] {
        assert_eq!(decode::<u32>(&v.to_bytes(V).unwrap()).unwrap(), v);
    }
}

// --- bytes / String ---------------------------------------------------------

#[test]
fn parse_bytes_is_u32_length_prefixed() {
    // Empty, one byte, 512 bytes, 255 bytes.
    assert_eq!(
        decode::<Vec<u8>>(b"\x00\x00\x00\x00").unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        decode::<Vec<u8>>(b"\x00\x00\x00\x01\xff").unwrap(),
        vec![0xff]
    );
    let big = [b"\x00\x00\x02\x00".as_slice(), &[b'a'; 512]].concat();
    assert_eq!(decode::<Vec<u8>>(&big).unwrap(), vec![b'a'; 512]);
    // Declared length longer than available (off-by-one) must reject, not truncate.
    let short = [b"\x00\x00\x02\x01".as_slice(), &[b'a'; 512]].concat();
    assert!(
        decode::<Vec<u8>>(&short).is_err(),
        "declared 513 with 512 present"
    );
    // Length prefix present, no payload.
    assert!(decode::<Vec<u8>>(b"\x00\x00\x00\xff").is_err());
}

#[test]
fn parse_str_is_u32_length_prefixed() {
    assert_eq!(decode::<String>(b"\x00\x00\x00\x00").unwrap(), "");
    assert_eq!(decode::<String>(b"\x00\x00\x00\x01a").unwrap(), "a");
    let b255 = [b"\x00\x00\x00\xff".as_slice(), &[b'b'; 255]].concat();
    assert_eq!(decode::<String>(&b255).unwrap(), "b".repeat(255));
    // EOF off-by-one: declares 513, provides 512.
    let short = [b"\x00\x00\x02\x01".as_slice(), &[b'a'; 512]].concat();
    assert!(decode::<String>(&short).is_err());
    // Declared 255, only 3 bytes present.
    assert!(decode::<String>(b"\x00\x00\x00\xff\x01\x02\x03").is_err());
}

// A hostile u32 String length (~4.29e9) with no payload must return Err from the remaining-bytes
// guard BEFORE the `vec![0u8; len]` zero-fills multiple GiB (allocation-bomb class). If this
// test ever allocates 4 GiB it will OOM the runner rather than pass — that is the point.
#[test]
fn oversized_string_length_rejects_without_allocating() {
    assert!(decode::<String>(b"\xff\xff\xff\xff").is_err());
    assert!(decode::<String>(b"\xff\xff\xff\xff\x61\x62").is_err());
}

#[test]
fn string_round_trips() {
    for s in ["", "a", &"b".repeat(255)] {
        let s = s.to_string();
        assert_eq!(decode::<String>(&s.to_bytes(V).unwrap()).unwrap(), s);
    }
}

// --- Option -----------------------------------------------------------------

#[test]
fn parse_optional_present_and_absent() {
    // 0x00 -> None; 0x01 <bool> -> Some(bool).
    assert_eq!(decode::<Option<bool>>(b"\x00").unwrap(), None);
    assert_eq!(decode::<Option<bool>>(b"\x01\x01").unwrap(), Some(true));
    assert_eq!(decode::<Option<bool>>(b"\x01\x00").unwrap(), Some(false));
    // Presence tag says Some but the inner value is missing -> EOF reject.
    assert!(decode::<Option<bool>>(b"\x01").is_err());
    // The inner type itself may reject: Some(bool) with an invalid bool byte.
    assert!(decode::<Option<bool>>(b"\x01\x02").is_err());
}

#[test]
fn option_round_trips() {
    for o in [None, Some(0u32), Some(4_294_967_295u32)] {
        assert_eq!(decode::<Option<u32>>(&o.to_bytes(V).unwrap()).unwrap(), o);
    }
}

// --- Vec / list -------------------------------------------------------------

#[test]
fn parse_list_is_u32_count_prefixed() {
    assert_eq!(
        decode::<Vec<bool>>(b"\x00\x00\x00\x00").unwrap(),
        Vec::<bool>::new()
    );
    assert_eq!(
        decode::<Vec<bool>>(b"\x00\x00\x00\x01\x01").unwrap(),
        vec![true]
    );
    assert_eq!(
        decode::<Vec<bool>>(b"\x00\x00\x00\x03\x01\x00\x01").unwrap(),
        vec![true, false, true]
    );
    // Count says 1, no element present -> EOF reject.
    assert!(decode::<Vec<bool>>(b"\x00\x00\x00\x01").is_err());
    // Count says 255, two elements present -> EOF reject.
    assert!(decode::<Vec<bool>>(b"\x00\x00\x00\xff\x00\x00").is_err());
    // Element decode failure propagates (invalid bool inside a 1-element list).
    assert!(decode::<Vec<bool>>(b"\x00\x00\x00\x01\x02").is_err());
}

// A hostile u32 count (~4.29e9) with an empty payload must return Err on the first missing element,
// never panic and never hang materially: the loop errors on the first `from_bytes`.
#[test]
fn oversized_list_count_rejects_without_panicking() {
    assert!(decode::<Vec<bool>>(b"\xff\xff\xff\xff").is_err());
    assert!(decode::<Vec<bool>>(b"\xff\xff\xff\xff\x00\x00").is_err());
}

// --- tuple ------------------------------------------------------------------

#[test]
fn parse_tuple_is_concatenated_fields() {
    assert_eq!(decode::<(bool, bool)>(b"\x00\x00").unwrap(), (false, false));
    assert_eq!(decode::<(bool, bool)>(b"\x00\x01").unwrap(), (false, true));
    // Inner-type failure propagates.
    assert!(decode::<(bool, bool)>(b"\x00\x02").is_err());
    // EOF: only one of two fields present.
    assert!(decode::<(bool, bool)>(b"\x00").is_err());
}

// --- composite round-trip ---------------------------------------------------

#[test]
fn nested_composite_round_trips_byte_for_byte() {
    // list<list<u32>>, a pair of optionals (present/absent), and a (u32, String, bytes) triple,
    // composed from the 2- and 3-tuple `ChiaSerialize` impls.
    type T = (
        Vec<Vec<u32>>,
        (Option<u32>, Option<u32>),
        (u32, String, Vec<u8>),
    );
    let value: T = (
        vec![vec![1, 2, 3], vec![3, 4]],
        (Some(728), None),
        (383, "hello".to_string(), b"goodbye".to_vec()),
    );
    let bytes = value.to_bytes(V).unwrap();
    let back: T = decode(&bytes).unwrap();
    assert_eq!(back, value, "structural round-trip");
    assert_eq!(back.to_bytes(V).unwrap(), bytes, "byte-exact round-trip");
}

// --- Coin: exact-buffer consumption -----------------------------------------------------------

#[test]
fn coin_round_trips_and_consumes_exact_bytes() {
    let coin = Coin {
        parent_coin_info: Bytes32::from([0u8; 32]),
        puzzle_hash: Bytes32::from([0u8; 32]),
        amount: 0,
    };
    let bytes = coin.to_bytes(V).unwrap();
    assert_eq!(bytes.len(), 72, "coin is 32 + 32 + 8 bytes on the wire");

    let mut cur = Cursor::new(bytes.as_slice());
    let back = Coin::from_bytes(&mut cur, V).unwrap();
    assert_eq!(back, coin);
    assert_eq!(
        cur.position() as usize,
        bytes.len(),
        "an exactly-sized coin buffer is fully consumed"
    );
}

// ===========================================================================
// Canonical strictness checks
// ===========================================================================

// The presence tag must be exactly 0x00 or 0x01; a malformed tag (0x02..=0xff) is rejected.
#[test]
fn option_presence_tag_must_be_zero_or_one_like_chia() {
    // \x02\x00 -> Err.
    assert!(
        decode::<Option<bool>>(b"\x02\x00").is_err(),
        "presence tag 0x02 must be rejected"
    );
    assert!(
        decode::<Option<bool>>(b"\xff\x00").is_err(),
        "presence tag 0xff must be rejected"
    );
    // The canonical tags still decode: 0x00 -> None, 0x01 <inner> -> Some(inner).
    assert_eq!(decode::<Option<bool>>(b"\x00").unwrap(), None);
    assert_eq!(decode::<Option<bool>>(b"\x01\x01").unwrap(), Some(true));
    assert_eq!(decode::<Option<bool>>(b"\x01\x00").unwrap(), Some(false));
}

// The top-level `from_bytes(blob)` must reject a buffer with trailing bytes.
// `ChiaSerialize::from_bytes` is
// cursor-based and consumes only what the type needs, leaving trailing bytes un-inspected.
#[test]
fn from_bytes_rejects_trailing_bytes_like_chia() {
    let coin = Coin {
        parent_coin_info: Bytes32::from([0u8; 32]),
        puzzle_hash: Bytes32::from([0u8; 32]),
        amount: 0,
    };
    let valid = coin.to_bytes(V).unwrap();
    let mut padded = valid.clone();
    padded.push(0x00); // one trailing byte

    // The cursor-based reader still decodes the valid prefix and stops
    // at 72 — nested composite decoding depends on this, so it must NOT reject trailing bytes.
    let mut cur = Cursor::new(padded.as_slice());
    let _ = Coin::from_bytes(&mut cur, V).expect("valid prefix still decodes");
    assert_eq!(
        cur.position() as usize,
        72,
        "the nested reader consumes only the coin's own bytes"
    );

    // The top-level entry requires full consumption: the trailing
    // byte is rejected, while an exactly-sized buffer still decodes through the same entry.
    assert!(
        Coin::from_bytes_exact(&padded, V).is_err(),
        "chia rejects trailing bytes at the top-level from_bytes"
    );
    assert_eq!(
        Coin::from_bytes_exact(&valid, V).expect("exact buffer decodes"),
        coin,
        "an exactly-sized buffer decodes through the outer entry"
    );
}

// Coverage for the from_bytes/parse split: the full-consumption check lives ONLY at
// the outer `from_bytes_exact`, for both a primitive and a composite, and NEVER inside nested field
// parsing (a None/Some Option legitimately hands the following bytes to the next tuple field).
#[test]
fn from_bytes_exact_rejects_trailing_bytes_for_primitive_and_composite() {
    // Primitive: a bare u32 plus one trailing byte.
    let mut u32_padded = 7u32.to_bytes(V).unwrap();
    assert_eq!(u32::from_bytes_exact(&u32_padded, V).unwrap(), 7);
    u32_padded.push(0x00);
    assert!(
        u32::from_bytes_exact(&u32_padded, V).is_err(),
        "primitive: trailing byte rejected at outer entry"
    );

    // Composite: the nested tuple shape from the round-trip test, plus one trailing byte.
    type T = (
        Vec<Vec<u32>>,
        (Option<u32>, Option<u32>),
        (u32, String, Vec<u8>),
    );
    let value: T = (
        vec![vec![1, 2, 3], vec![3, 4]],
        (Some(728), None),
        (383, "hello".to_string(), b"goodbye".to_vec()),
    );
    let exact = value.to_bytes(V).unwrap();
    assert_eq!(T::from_bytes_exact(&exact, V).unwrap(), value);
    let mut padded = exact.clone();
    padded.push(0x00);
    assert!(
        T::from_bytes_exact(&padded, V).is_err(),
        "composite: trailing byte rejected at outer entry"
    );
}

#[test]
fn nested_optional_leaves_bytes_for_next_field() {
    // The crux of the from_bytes/parse split: a nested Option must NOT assert full consumption —
    // it hands the bytes after its own value to the next field. Both a None (1-byte tag) and a
    // Some(u32) (5-byte) Option round-trip through the outer entry, and decoding the leading Option
    // on its own via the cursor reader leaves the trailing u32 for the next field to read.
    type Pair = (Option<u32>, u32);
    for value in [(None, 7u32), (Some(9u32), 7u32)] {
        let bytes = value.to_bytes(V).unwrap();
        assert_eq!(
            Pair::from_bytes_exact(&bytes, V).unwrap(),
            value,
            "outer entry consumes the whole pair"
        );
        let mut cur = Cursor::new(bytes.as_slice());
        let opt = Option::<u32>::from_bytes(&mut cur, V).unwrap();
        assert_eq!(opt, value.0, "nested Option decodes only its own bytes");
        let rest = u32::from_bytes(&mut cur, V).unwrap();
        assert_eq!(rest, value.1, "the following u32 is still readable");
    }
}
