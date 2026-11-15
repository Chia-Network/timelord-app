//! Byte-parity gate for the wallet-facing `MerkleSet` proof encoding: every proof this
//! implementation generates must be BYTE-EQUAL to the reference proofs in the vendored
//! fixture — a light wallet verifies these bytes against the block header's foliage roots,
//! so near-enough is not enough.
//!
//! Fixture: `fixtures/merkle_set_proofs_chia_rs_0_42_1.txt`, emitted by an oracle script
//! over the fixed leaf sets below. Line format: `case <name>` / `leaf <hex>`* /
//! `root <hex>` / `probe <item> <0|1> <proof-hex>`*.

use dg_xch_core::consensus::merkle_set::{MerkleSet, validate_merkle_proof};

fn hx32(s: &str) -> [u8; 32] {
    let v = hex::decode(s).expect("hex");
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    out
}

struct Probe {
    item: [u8; 32],
    included: bool,
    proof: Vec<u8>,
}

struct Case {
    name: String,
    leafs: Vec<[u8; 32]>,
    root: [u8; 32],
    probes: Vec<Probe>,
}

fn load_cases() -> Vec<Case> {
    let raw = include_str!("fixtures/merkle_set_proofs_chia_rs_0_42_1.txt");
    let mut cases: Vec<Case> = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("case") => cases.push(Case {
                name: parts.next().expect("case name").to_string(),
                leafs: Vec::new(),
                root: [0u8; 32],
                probes: Vec::new(),
            }),
            Some("leaf") => {
                let case = cases.last_mut().expect("leaf before case");
                case.leafs.push(hx32(parts.next().expect("leaf hex")));
            }
            Some("root") => {
                let case = cases.last_mut().expect("root before case");
                case.root = hx32(parts.next().expect("root hex"));
            }
            Some("probe") => {
                let case = cases.last_mut().expect("probe before case");
                case.probes.push(Probe {
                    item: hx32(parts.next().expect("probe item")),
                    included: parts.next().expect("probe flag") == "1",
                    proof: hex::decode(parts.next().expect("probe proof")).expect("proof hex"),
                });
            }
            Some(other) => panic!("unknown fixture line tag {other}"),
            None => {}
        }
    }
    assert!(!cases.is_empty(), "fixture parsed no cases");
    cases
}

// Every case: our root equals the fixture root; every probe: our (included, proof-bytes)
// equals the fixture output EXACTLY, and the proof round-trip verifies against the root
// (inclusion AND exclusion).
#[test]
fn proofs_are_byte_equal_to_chia_rs_0_42_1() {
    let mut probes_checked = 0usize;
    for case in load_cases() {
        let tree = MerkleSet::from_leafs(&mut case.leafs.clone());
        assert_eq!(
            tree.get_root(),
            case.root,
            "root mismatch in case {}",
            case.name
        );
        for probe in &case.probes {
            let (included, proof) = tree.generate_proof(&probe.item).expect("generate proof");
            assert_eq!(
                included, probe.included,
                "inclusion verdict mismatch in case {}",
                case.name
            );
            assert_eq!(
                hex::encode(&proof),
                hex::encode(&probe.proof),
                "proof BYTES diverge from chia_rs 0.42.1 in case {} (item {})",
                case.name,
                hex::encode(probe.item)
            );
            assert_eq!(
                validate_merkle_proof(&proof, &probe.item, &case.root),
                Ok(probe.included),
                "round-trip verification failed in case {}",
                case.name
            );
            probes_checked += 1;
        }
    }
    assert!(probes_checked >= 40, "fixture corpus unexpectedly small");
}
