use dg_xch_core::formatting::{bigint_to_bytes, number_from_slice};
use num_bigint::BigInt;

fn reference(v: &BigInt) -> Vec<u8> {
    if v == &BigInt::from(0) {
        return vec![];
    }
    let mut b = v.to_signed_bytes_be();
    while b.len() > 1 && ((b[0] == 0x00 && b[1] & 0x80 == 0) || (b[0] == 0xFF && b[1] & 0x80 != 0))
    {
        b.remove(0);
    }
    b
}

#[test]
fn positive_word_boundary_value_gets_sign_pad_byte() {
    let v = BigInt::from(0xCECD_48C0u32);
    assert_eq!(
        bigint_to_bytes(&v, true),
        vec![0x00, 0xCE, 0xCD, 0x48, 0xC0],
        "positive 4-byte value with high bit set must gain a 0x00 pad, not a copy of its low byte"
    );
}

#[test]
fn signed_encoding_matches_chia_reference_across_boundaries() {
    let mut cases: Vec<BigInt> = Vec::new();
    for shift in [7u32, 8, 15, 16, 31, 32, 39, 63, 64, 95, 96, 127, 128, 159] {
        let p = BigInt::from(1) << shift;
        for delta in [-2i64, -1, 0, 1, 2] {
            cases.push(&p + BigInt::from(delta));
            cases.push(-(&p + BigInt::from(delta)));
        }
    }
    for byte in [0x7F_u8, 0x80, 0xFF] {
        for len in 1usize..=20 {
            cases.push(BigInt::from_signed_bytes_be(
                &std::iter::once(0x00)
                    .chain(std::iter::repeat_n(byte, len))
                    .collect::<Vec<u8>>(),
            ));
        }
    }
    for v in &cases {
        let got = bigint_to_bytes(v, true);
        let want = reference(v);
        assert_eq!(got, want, "encode mismatch for {v}");
        assert_eq!(&number_from_slice(&got), v, "round-trip mismatch for {v}");
    }
}
