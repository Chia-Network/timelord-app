use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

const V: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

struct Golden {
    height: u32,
    header_hash: Bytes32,
    record: Vec<u8>,
}

fn goldens() -> Vec<Golden> {
    let text = include_str!("fixtures/block_record_mainnet_3000000.txt");
    let mut out = Vec::new();
    let mut height: Option<u32> = None;
    let mut hash: Option<Bytes32> = None;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("HEIGHT ") {
            height = Some(h.trim().parse().expect("height"));
        } else if let Some(h) = line.strip_prefix("HASH ") {
            let raw: [u8; 32] = hex::decode(h.trim())
                .expect("hash hex")
                .try_into()
                .expect("32 bytes");
            hash = Some(Bytes32::from(raw));
        } else if let Some(r) = line.strip_prefix("RECORD ") {
            out.push(Golden {
                height: height.take().expect("HEIGHT before RECORD"),
                header_hash: hash.take().expect("HASH before RECORD"),
                record: hex::decode(r.trim()).expect("record hex"),
            });
        }
    }
    assert_eq!(out.len(), 5, "fixture should hold five golden records");
    out
}

fn decode(blob: &[u8]) -> BlockRecord {
    let mut cur = Cursor::new(blob);
    let rec = BlockRecord::from_bytes(&mut cur, V).expect("chia mainnet record must decode");
    assert_eq!(
        cur.position(),
        blob.len() as u64,
        "decode must consume the record exactly"
    );
    rec
}

/// The parser must accept real serialized records and land every field where the wire put it.
#[test]
fn block_record_decodes_chia_mainnet_bytes() {
    for g in goldens() {
        let rec = decode(&g.record);
        assert_eq!(rec.height, g.height);
        assert_eq!(rec.header_hash, g.header_hash);
        // The stored blob's first 32 bytes are the record's own header_hash.
        assert_eq!(&g.record[..32], AsRef::<[u8]>::as_ref(&g.header_hash));
    }
}

/// And re-emit them byte-identically: encode(decode(bytes)) == bytes.
#[test]
fn block_record_reencodes_chia_mainnet_bytes_identically() {
    for g in goldens() {
        let rec = decode(&g.record);
        let ours = rec.to_bytes(V).expect("encode");
        assert_eq!(
            hex::encode(&ours),
            hex::encode(&g.record),
            "height {} must round-trip byte-exact",
            g.height
        );
    }
}

#[test]
fn golden_set_covers_the_variable_layout() {
    let recs: Vec<BlockRecord> = goldens().iter().map(|g| decode(&g.record)).collect();
    assert!(
        recs.iter()
            .any(|r| r.infused_challenge_vdf_output.is_some()),
        "need an infused challenge output"
    );
    assert!(
        recs.iter().any(|r| r.timestamp.is_some()),
        "need a transaction block"
    );
    assert!(
        recs.iter().any(|r| r.timestamp.is_none()),
        "need a non-transaction block"
    );
    assert!(
        recs.iter().any(|r| r
            .reward_claims_incorporated
            .as_ref()
            .is_some_and(|c| !c.is_empty())),
        "need incorporated reward claims"
    );
    assert!(
        recs.iter().any(|r| r.sub_epoch_summary_included.is_some()),
        "need a sub-epoch-summary boundary record"
    );
}
