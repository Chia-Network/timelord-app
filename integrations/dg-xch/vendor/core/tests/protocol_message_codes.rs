// Message-type completeness against the 2.7.1 protocol, including the `error` message.
//
// Completeness is load-bearing: the read loop disconnects a peer that sends an undefined message
// type, which is only safe if our recognized-code set EQUALS the protocol's. A defined code we
// map to `Unknown` would make us ban conforming peers over an inbound `error` frame or a
// `configure_window_sizes` handshake follow-up.

use dg_xch_core::protocols::ProtocolMessageTypes;
use dg_xch_core::protocols::shared::ErrorMessage;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

/// Every message-type code the 2.7.1 protocol defines (all 99 members).
const CHIA_2_7_1_CODES: [u8; 99] = [
    1, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
    52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75,
    76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99,
    100, 101,
];
/// The tail defined above 101: mempool updates (104-107), solution_response (108),
/// solve (109), partial_proofs (110), configure_window_sizes (111), error (255).
const CHIA_2_7_1_CODES_TAIL: [u8; 10] = [102, 103, 104, 105, 106, 107, 108, 109, 110, 111];
const CHIA_ERROR_CODE: u8 = 255;

// Every code the 2.7.1 protocol recognizes must round-trip through our enum, never collapsing
// to the `Unknown` sentinel.
#[test]
fn every_chia_2_7_1_code_is_recognized() {
    for code in CHIA_2_7_1_CODES
        .iter()
        .chain(CHIA_2_7_1_CODES_TAIL.iter())
        .chain([CHIA_ERROR_CODE].iter())
    {
        let t = ProtocolMessageTypes::from(*code);
        assert_ne!(
            t,
            ProtocolMessageTypes::Unknown,
            "code {code} must be recognized (the unknown-type disconnect depends on the sets matching)"
        );
        assert_eq!(
            t as u8, *code,
            "code {code} must round-trip through the enum"
        );
    }
}

// Undefined codes must still collapse to `Unknown` — the disconnect set must not shrink
// either. (0 and 2 are unassigned; 4 was retired for 66; 112..255-exclusive are unassigned.)
#[test]
fn codes_chia_does_not_define_stay_unknown() {
    for code in [0u8, 2, 4, 112, 150, 200, 254] {
        assert_eq!(
            ProtocolMessageTypes::from(code),
            ProtocolMessageTypes::Unknown,
            "code {code} is not a defined message type and must map to Unknown"
        );
    }
}

// The `error` message body is int16 code, str message, Optional[bytes] data. It must decode
// from that exact wire shape and round-trip byte-identically.
#[test]
fn error_message_wire_shape_matches_chia() {
    let v = ChiaProtocolVersion::default();
    // Hand-built wire encoding: int16 BE -13, "no fee" (u32 len + utf8), None tag.
    let mut chia_bytes: Vec<u8> = Vec::new();
    chia_bytes.extend((-13i16).to_be_bytes());
    chia_bytes.extend(6u32.to_be_bytes());
    chia_bytes.extend(b"no fee");
    chia_bytes.push(0u8);

    let decoded = ErrorMessage::from_bytes(&mut Cursor::new(chia_bytes.as_slice()), v)
        .expect("wire-shaped Error decodes");
    assert_eq!(decoded.code, -13);
    assert_eq!(decoded.message, "no fee");
    assert_eq!(decoded.data, None);
    assert_eq!(
        decoded.to_bytes(v).unwrap(),
        chia_bytes,
        "round-trip is byte-identical"
    );

    // The Some(data) arm: bytes stream as u32 length + raw bytes — Vec<u8>'s wire shape.
    let with_data = ErrorMessage {
        code: 2,
        message: "detail".to_string(),
        data: Some(vec![0xDE, 0xAD]),
    };
    let bytes = with_data.to_bytes(v).unwrap();
    let back = ErrorMessage::from_bytes(&mut Cursor::new(bytes.as_slice()), v).unwrap();
    assert_eq!(back, with_data);
}
