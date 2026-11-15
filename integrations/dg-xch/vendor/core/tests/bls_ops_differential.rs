#![cfg(feature = "bls")]

use bls12_381::{G1Affine, G1Projective, Scalar};
use dg_xch_core::clvm::arena::{Arena, NodePtr};
use dg_xch_core::clvm::dialect::ChiaDialect;
use dg_xch_core::clvm::more_ops::{op_point_add, op_pubkey_for_exp};
use dg_xch_core::errors::ClvmError;
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const SEED: u64 = 0x0DDB_1A5E_D0DD_5EED;

const MALLOC_COST_PER_BYTE: u64 = 10;
const PUBKEY_BASE_COST: u64 = 1_325_730;
const PUBKEY_COST_PER_BYTE: u64 = 38;
const POINT_ADD_BASE_COST: u64 = 101_094;
const POINT_ADD_COST_PER_ARG: u64 = 1_343_980;
const INFINITE_COST: u64 = 0x7FFF_FFFF_FFFF_FFFF;

fn group_order() -> BigInt {
    let order_as_bytes = &[
        0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1, 0xd8,
        0x05, 0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
        0x00, 0x01,
    ];
    BigUint::from_bytes_be(order_as_bytes).into()
}

fn mod_group_order(n: &BigInt) -> BigInt {
    let order = group_order();
    let mut remainder = n.mod_floor(&order);
    if remainder.sign() == Sign::Minus {
        remainder += order;
    }
    remainder
}

fn number_to_scalar(n: &BigInt) -> Scalar {
    let (sign, as_u8): (Sign, Vec<u8>) = n.to_bytes_le();
    let mut scalar_array: [u8; 32] = [0; 32];
    scalar_array[..as_u8.len()].clone_from_slice(&as_u8[..]);
    let exp: Scalar = Scalar::from_bytes(&scalar_array).unwrap();
    if sign == Sign::Minus { -exp } else { exp }
}

/// Signed big-endian atom decode — `number_from_slice` (empty => 0).
fn number_from_slice(v: &[u8]) -> BigInt {
    if v.is_empty() {
        BigInt::from(0)
    } else {
        BigInt::from_signed_bytes_be(v)
    }
}

enum PubkeyOracleOutcome {
    Ok(u64, [u8; 48]),
    /// `check_cost` fired before the scalar multiplication.
    CostExceeded,
}

fn oracle_pubkey_for_exp(atom: &[u8], max_cost: u64) -> PubkeyOracleOutcome {
    let cost = PUBKEY_BASE_COST + (atom.len() as u64) * PUBKEY_COST_PER_BYTE;
    if cost > max_cost {
        return PubkeyOracleOutcome::CostExceeded;
    }
    let exp = mod_group_order(&number_from_slice(atom));
    let exp: Scalar = number_to_scalar(&exp);
    let point: G1Projective = G1Affine::generator() * exp;
    let point: G1Affine = point.into();
    PubkeyOracleOutcome::Ok(cost + 48 * MALLOC_COST_PER_BYTE, point.to_compressed())
}

enum OracleOutcome {
    Ok(u64, [u8; 48]),
    /// Wrong-length atom.
    WrongLength,
    InvalidPoint,
    /// The running cost exceeded max_cost.
    CostExceeded,
}

fn oracle_point_add(atoms: &[Vec<u8>], max_cost: u64) -> OracleOutcome {
    let mut cost = POINT_ADD_BASE_COST;
    let mut total: G1Projective = G1Projective::identity();
    for blob in atoms {
        cost += POINT_ADD_COST_PER_ARG;
        if cost > max_cost {
            return OracleOutcome::CostExceeded;
        }
        if blob.len() != 48 {
            return OracleOutcome::WrongLength;
        }
        let mut as_array: [u8; 48] = [0; 48];
        as_array.clone_from_slice(&blob[0..48]);
        let v = G1Affine::from_compressed(&as_array);
        if bool::from(v.is_some()) {
            let point = v.unwrap();
            total += &point;
        } else {
            return OracleOutcome::InvalidPoint;
        }
    }
    let total: G1Affine = total.into();
    OracleOutcome::Ok(cost + 48 * MALLOC_COST_PER_BYTE, total.to_compressed())
}

// ---------------------------------------------------------------------------------------
// Driving the real operators through the arena.
// ---------------------------------------------------------------------------------------

fn atom_list(arena: &mut Arena, atoms: &[Vec<u8>]) -> NodePtr {
    let mut list = NodePtr::NIL;
    for blob in atoms.iter().rev() {
        let a = arena.new_atom(blob).expect("atom");
        list = arena.new_pair(a, list).expect("pair");
    }
    list
}

fn run_pubkey_for_exp(atom: &[u8], max_cost: u64) -> Result<(u64, Vec<u8>), ClvmError> {
    let mut arena = Arena::new();
    let dialect = ChiaDialect::new(0);
    let args = atom_list(&mut arena, &[atom.to_vec()]);
    let (cost, out) = op_pubkey_for_exp(&arena, args, max_cost, &dialect)?;
    let (cost, ptr) = out.materialize(&mut arena, cost)?;
    let bytes = arena.atom(ptr).expect("atom result").as_ref().to_vec();
    Ok((cost, bytes))
}

fn run_point_add(atoms: &[Vec<u8>], max_cost: u64) -> Result<(u64, Vec<u8>), ClvmError> {
    let mut arena = Arena::new();
    let dialect = ChiaDialect::new(0);
    let args = atom_list(&mut arena, atoms);
    let (cost, out) = op_point_add(&arena, args, max_cost, &dialect)?;
    let (cost, ptr) = out.materialize(&mut arena, cost)?;
    let bytes = arena.atom(ptr).expect("atom result").as_ref().to_vec();
    Ok((cost, bytes))
}

fn assert_point_add_matches(atoms: &[Vec<u8>], max_cost: u64, ctx: &str) {
    let new = run_point_add(atoms, max_cost);
    match oracle_point_add(atoms, max_cost) {
        OracleOutcome::Ok(cost, bytes) => {
            let (new_cost, new_bytes) = new.unwrap_or_else(|e| {
                panic!("point_add diverged ({ctx}): oracle Ok, new Err({e:?}) atoms={atoms:02x?}")
            });
            assert_eq!(
                new_cost, cost,
                "point_add cost diverged ({ctx}): {atoms:02x?}"
            );
            assert_eq!(
                new_bytes,
                bytes.to_vec(),
                "point_add bytes diverged ({ctx}): {atoms:02x?}"
            );
        }
        OracleOutcome::WrongLength | OracleOutcome::CostExceeded | OracleOutcome::InvalidPoint => {
            assert!(
                new.is_err(),
                "point_add diverged ({ctx}): oracle Err, new Ok atoms={atoms:02x?}"
            );
        }
    }
}

fn assert_pubkey_matches(atom: &[u8], max_cost: u64, ctx: &str) {
    match oracle_pubkey_for_exp(atom, max_cost) {
        PubkeyOracleOutcome::Ok(cost, bytes) => {
            let (new_cost, new_bytes) = run_pubkey_for_exp(atom, max_cost).unwrap_or_else(|e| {
                panic!("pubkey_for_exp errored ({ctx}): {e:?} atom={atom:02x?}")
            });
            assert_eq!(
                new_cost, cost,
                "pubkey_for_exp cost diverged ({ctx}): {atom:02x?}"
            );
            assert_eq!(
                new_bytes,
                bytes.to_vec(),
                "pubkey_for_exp bytes diverged ({ctx}): {atom:02x?}"
            );
        }
        PubkeyOracleOutcome::CostExceeded => {
            assert!(
                run_pubkey_for_exp(atom, max_cost).is_err(),
                "pubkey_for_exp diverged ({ctx}): oracle CostExceeded, new Ok atom={atom:02x?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// Case generators.
// ---------------------------------------------------------------------------------------

/// BLS12-381 group order r, big-endian.
const R_BE: [u8; 32] = [
    0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1, 0xd8, 0x05,
    0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01,
];

fn signed_be(n: &BigInt) -> Vec<u8> {
    n.to_signed_bytes_be()
}

fn fixed_exponent_corners() -> Vec<Vec<u8>> {
    let r: BigInt = BigUint::from_bytes_be(&R_BE).into();
    let mut cases: Vec<Vec<u8>> = vec![
        vec![],           // 0 (empty atom)
        vec![0x00],       // 0
        vec![0x01],       // 1
        vec![0xff],       // -1
        vec![0x80],       // -128
        vec![0x00, 0xff], // 255
        vec![0x7f; 32],   // large positive
        vec![0xff; 32],   // -1 over 32 bytes
        vec![0xff; 64],   // -1 over 64 bytes
        vec![0x00; 48],   // 0 over 48 bytes
        R_BE.to_vec(),    // exactly r (top byte 0x73: positive signed decode)
    ];
    for delta in [-2i64, -1, 0, 1, 2] {
        let v = &r + BigInt::from(delta);
        cases.push(signed_be(&v));
        cases.push(signed_be(&(-&v)));
        cases.push(signed_be(&(&v * 2)));
    }
    cases
}

fn random_valid_point(rng: &mut StdRng) -> [u8; 48] {
    let mut wide = [0u8; 64];
    rng.fill(&mut wide[..]);
    let scalar = Scalar::from_bytes_wide(&wide);
    let point: G1Affine = (G1Affine::generator() * scalar).into();
    point.to_compressed()
}

/// An on-curve compressed encoding outside the G1 subgroup, found by x-search.
fn wrong_subgroup_point(rng: &mut StdRng) -> [u8; 48] {
    loop {
        let mut candidate = [0u8; 48];
        rng.fill(&mut candidate[..]);
        candidate[0] = (candidate[0] & 0x3f) | 0x80; // compressed, not infinity, sort clear
        let parsed = G1Affine::from_compressed_unchecked(&candidate);
        if bool::from(parsed.is_some()) && !bool::from(parsed.unwrap().is_torsion_free()) {
            return candidate;
        }
    }
}

fn invalid_point_corners(rng: &mut StdRng) -> Vec<Vec<u8>> {
    let valid = random_valid_point(rng);
    let mut cases: Vec<Vec<u8>> = Vec::new();
    // Flag-bit corners over an otherwise-valid x and over zero tails.
    for first in [0x00u8, 0x20, 0x40, 0x60, 0xa0, 0xc0, 0xe0] {
        let mut v = valid.to_vec();
        v[0] = first | (v[0] & 0x1f);
        cases.push(v);
    }
    // Non-canonical infinity encodings.
    let mut inf_bad = vec![0u8; 48];
    inf_bad[0] = 0xc0;
    inf_bad[47] = 0x01;
    cases.push(inf_bad);
    cases.push({
        let mut v = vec![0u8; 48];
        v[0] = 0xe0;
        v
    });
    // x = 0 without the infinity flag (both sort-bit values).
    cases.push({
        let mut v = vec![0u8; 48];
        v[0] = 0x80;
        v
    });
    cases.push({
        let mut v = vec![0u8; 48];
        v[0] = 0xa0;
        v
    });
    // Non-canonical x >= p: p is < 2^381, so force the top field bits high.
    cases.push({
        let mut v = vec![0xffu8; 48];
        v[0] = 0x9f;
        v
    });
    // Wrong subgroup (on-curve, not in G1).
    cases.push(wrong_subgroup_point(rng).to_vec());
    cases.push(wrong_subgroup_point(rng).to_vec());
    cases
}

// ---------------------------------------------------------------------------------------
// The differential tests.
// ---------------------------------------------------------------------------------------

#[test]
fn pubkey_for_exp_matches_bls12_381_reference() {
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut cases = 0usize;
    for atom in fixed_exponent_corners() {
        assert_pubkey_matches(&atom, INFINITE_COST, "fixed corner");
        cases += 1;
    }
    for _ in 0..4000 {
        let len = rng.random_range(0..=64);
        let mut atom = vec![0u8; len];
        rng.fill(&mut atom[..]);
        assert_pubkey_matches(&atom, INFINITE_COST, "random");
        cases += 1;
    }
    println!("pubkey_for_exp differential: {cases} cases identical");
    assert!(cases > 4000);
}

#[test]
fn pubkey_for_exp_max_cost_boundaries_match() {
    for len in [0usize, 1, 32, 64] {
        let atom = vec![0x01u8; len];
        let boundary = PUBKEY_BASE_COST + (len as u64) * PUBKEY_COST_PER_BYTE;
        for delta in [-1i64, 0, 1] {
            let budget = (boundary as i64 + delta).max(0) as u64;
            assert_pubkey_matches(&atom, budget, "cost boundary");
        }
    }
    // At exactly the boundary the op succeeds and the returned cost carries the malloc
    // surcharge above the budget — the malloc share is not part of the in-op check.
    let boundary = PUBKEY_BASE_COST + PUBKEY_COST_PER_BYTE;
    let (cost, _) = run_pubkey_for_exp(&[0x01], boundary).expect("boundary budget succeeds");
    assert_eq!(cost, boundary + 48 * MALLOC_COST_PER_BYTE);
}

#[test]
fn pubkey_for_exp_arg_count_is_enforced() {
    // Zero args and two args error in both implementations (arg-count precedes math).
    let mut arena = Arena::new();
    let dialect = ChiaDialect::new(0);
    let args = NodePtr::NIL;
    assert!(op_pubkey_for_exp(&arena, args, INFINITE_COST, &dialect).is_err());
    let two = atom_list(&mut arena, &[vec![0x01], vec![0x02]]);
    assert!(op_pubkey_for_exp(&arena, two, INFINITE_COST, &dialect).is_err());
}

#[test]
fn point_add_single_blob_matches_bls12_381_reference() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 1);
    let mut cases = 0usize;

    // The canonical infinity, alone and repeated.
    let inf = {
        let mut v = vec![0u8; 48];
        v[0] = 0xc0;
        v
    };
    assert_point_add_matches(&[], INFINITE_COST, "empty list");
    assert_point_add_matches(std::slice::from_ref(&inf), INFINITE_COST, "infinity");
    assert_point_add_matches(&[inf.clone(), inf.clone()], INFINITE_COST, "infinity x2");
    cases += 3;

    // Every explicit invalid class, alone.
    for blob in invalid_point_corners(&mut rng) {
        assert_point_add_matches(&[blob], INFINITE_COST, "invalid corner");
        cases += 1;
    }

    // First-byte sweep over a zero tail: only 0xc0 is a point (infinity); everything else
    // must be classified identically (this pins the x = 0 guard against bls12_381).
    for first in 0u8..=255 {
        let mut v = vec![0u8; 48];
        v[0] = first;
        assert_point_add_matches(&[v], INFINITE_COST, "zero-tail sweep");
        cases += 1;
    }

    // Wrong lengths error identically.
    for len in [0usize, 1, 32, 47, 49, 96] {
        let mut v = vec![0u8; len];
        rng.fill(&mut v[..]);
        assert_point_add_matches(&[v], INFINITE_COST, "wrong length");
        cases += 1;
    }

    // Random valid points.
    for _ in 0..1000 {
        assert_point_add_matches(
            &[random_valid_point(&mut rng).to_vec()],
            INFINITE_COST,
            "valid point",
        );
        cases += 1;
    }

    // Arbitrary 48-byte fuzz: whatever the class, outcome and bytes must match, no panics.
    for _ in 0..3000 {
        let mut v = vec![0u8; 48];
        rng.fill(&mut v[..]);
        assert_point_add_matches(&[v], INFINITE_COST, "48-byte fuzz");
        cases += 1;
    }
    println!("point_add single-blob differential: {cases} cases identical");
    assert!(cases > 4000);
}

#[test]
fn point_add_argument_lists_match_bls12_381_reference() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 2);
    let mut corners = invalid_point_corners(&mut rng);
    corners.push({
        let mut v = vec![0u8; 48];
        v[0] = 0xc0;
        v
    });
    let mut cases = 0usize;
    for _ in 0..1500 {
        let n = rng.random_range(0..=6);
        let mut atoms = Vec::with_capacity(n);
        for _ in 0..n {
            match rng.random_range(0..4u8) {
                0 => atoms.push(random_valid_point(&mut rng).to_vec()),
                1 => atoms.push(corners[rng.random_range(0..corners.len())].clone()),
                2 => {
                    let mut v = vec![0u8; 48];
                    rng.fill(&mut v[..]);
                    atoms.push(v);
                }
                _ => {
                    // Occasionally a wrong length to exercise the error path mid-list.
                    if rng.random_range(0..8u8) == 0 {
                        atoms.push(vec![0u8; 47]);
                    } else {
                        atoms.push(random_valid_point(&mut rng).to_vec());
                    }
                }
            }
        }
        assert_point_add_matches(&atoms, INFINITE_COST, "mixed list");
        cases += 1;
    }
    println!("point_add list differential: {cases} cases identical");
    assert!(cases >= 1500);
}

#[test]
fn point_add_max_cost_exhaustion_matches() {
    let mut rng = StdRng::seed_from_u64(SEED ^ 3);
    let points: Vec<Vec<u8>> = (0..4)
        .map(|_| random_valid_point(&mut rng).to_vec())
        .collect();
    // Sweep max_cost across every per-arg boundary: below base, at base, between each arg.
    for args_priced in 0..=4u64 {
        for delta in [-1i64, 0, 1] {
            let budget =
                (POINT_ADD_BASE_COST + args_priced * POINT_ADD_COST_PER_ARG) as i64 + delta;
            assert_point_add_matches(&points, budget.max(0) as u64, "cost boundary");
        }
    }
    // An invalid point consumes budget like any other argument (charged before parse) and
    // then errors the operator — both sides must classify identically.
    let mut with_invalid = points.clone();
    with_invalid.insert(2, {
        let mut v = vec![0u8; 48];
        v[0] = 0x80;
        v
    });
    let budget = POINT_ADD_BASE_COST + 4 * POINT_ADD_COST_PER_ARG;
    assert_point_add_matches(&with_invalid, budget, "invalid consumes budget then errors");
}

#[test]
fn point_add_rejects_invalid_point_like_clvmr() {
    let mut garbage = vec![0u8; 48];
    garbage[0] = 0x80; // x = 0 without infinity: invalid in every implementation
    assert!(run_point_add(&[garbage], INFINITE_COST).is_err());

    let mut rng = StdRng::seed_from_u64(SEED ^ 24);
    let valid = random_valid_point(&mut rng).to_vec();
    let mut invalid_seen = 0usize;
    for corner in invalid_point_corners(&mut rng) {
        let mut as_array = [0u8; 48];
        as_array.clone_from_slice(&corner);
        if bool::from(G1Affine::from_compressed(&as_array).is_some()) {
            continue; // a valid encoding — covered by the differential tests
        }
        invalid_seen += 1;
        assert!(
            run_point_add(std::slice::from_ref(&corner), INFINITE_COST).is_err(),
            "invalid corner accepted alone: {corner:02x?}"
        );
        assert!(
            run_point_add(
                &[valid.clone(), corner.clone(), valid.clone()],
                INFINITE_COST
            )
            .is_err(),
            "invalid corner accepted mid-list: {corner:02x?}"
        );
    }
    assert!(invalid_seen >= 10, "corner corpus lost its invalid classes");
}

#[test]
fn point_add_charges_per_arg_before_parse_like_clvmr() {
    let mut garbage = vec![0u8; 48];
    garbage[0] = 0x80;
    let budget = POINT_ADD_BASE_COST; // no headroom for any per-arg cost
    assert!(run_point_add(std::slice::from_ref(&garbage), budget).is_err());

    // Same for a wrong-length argument: charged before its length is even inspected.
    assert!(run_point_add(&[vec![0u8; 47]], budget).is_err());

    // With exactly one arg of headroom the invalid argument passes check_cost and then
    // fails the parse — still an error, one branch later.
    let budget = POINT_ADD_BASE_COST + POINT_ADD_COST_PER_ARG;
    assert!(run_point_add(std::slice::from_ref(&garbage), budget).is_err());
}

#[test]
fn pubkey_for_exp_enforces_max_cost_like_clvmr() {
    let mut arena = Arena::new();
    let dialect = ChiaDialect::new(0);
    let args = atom_list(&mut arena, &[vec![0x01]]);
    assert!(op_pubkey_for_exp(&arena, args, 1000, &dialect).is_err());
}

// ---------------------------------------------------------------------------------------
// Inverse gates — each pins an outcome the PRE-FIX implementation produced
// (verified red against the pre-fix code) and asserts it is now unreachable.
// ---------------------------------------------------------------------------------------

#[test]
fn old_silent_skip_outcome_is_unreachable() {
    // Pre-fix: a lone garbage 48-byte point was skipped, yielding
    // Ok(POINT_ADD_BASE_COST + 480, canonical infinity). That Ok is now an error.
    let mut garbage = vec![0u8; 48];
    garbage[0] = 0x80;
    let res = run_point_add(&[garbage], INFINITE_COST);
    match res {
        Err(_) => {}
        Ok((cost, bytes)) => panic!(
            "old silent-skip outcome resurfaced: Ok(cost={cost}, bytes={bytes:02x?}) — \
             pre-fix returned Ok({}, infinity)",
            POINT_ADD_BASE_COST + 48 * MALLOC_COST_PER_BYTE
        ),
    }
}

#[test]
fn old_free_invalid_arg_outcome_is_unreachable() {
    // Pre-fix: an invalid argument consumed no budget, so base-only budget returned Ok.
    // Now the per-arg charge precedes the parse and the budget is exceeded.
    let mut garbage = vec![0u8; 48];
    garbage[0] = 0x80;
    assert!(
        run_point_add(&[garbage], POINT_ADD_BASE_COST).is_err(),
        "old cost-after-parse outcome resurfaced: invalid arg consumed no budget"
    );
}

#[test]
fn old_unmetered_pubkey_outcome_is_unreachable() {
    // Pre-fix: max_cost was ignored entirely — a budget of 0 still computed the point.
    assert!(
        run_pubkey_for_exp(&[0x01], 0).is_err(),
        "old unmetered pubkey_for_exp outcome resurfaced: budget 0 returned Ok"
    );
}
