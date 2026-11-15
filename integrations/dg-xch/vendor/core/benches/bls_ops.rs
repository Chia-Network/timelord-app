//! Old-vs-new bench for the two CLVM BLS operators: the previous bls12_381 implementation
//! (reconstructed byte-level, bls12_381 is a dev-dependency) against the shipped blst-backed
//! operators driven through the arena. The G1 math dominates both sides; arena overhead on
//! the new side is the real operator dispatch cost and is included deliberately.

use bls12_381::{G1Affine, G1Projective, Scalar};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use dg_xch_core::clvm::arena::{Arena, NodePtr};
use dg_xch_core::clvm::dialect::ChiaDialect;
use dg_xch_core::clvm::more_ops::{op_point_add, op_pubkey_for_exp};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::hint::black_box;

const INFINITE_COST: u64 = 0x7FFF_FFFF_FFFF_FFFF;

fn group_order() -> BigInt {
    let order_as_bytes = &[
        0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1, 0xd8,
        0x05, 0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
        0x00, 0x01,
    ];
    BigUint::from_bytes_be(order_as_bytes).into()
}

fn old_pubkey_for_exp(atom: &[u8]) -> [u8; 48] {
    let order = group_order();
    let n = if atom.is_empty() {
        BigInt::from(0)
    } else {
        BigInt::from_signed_bytes_be(atom)
    };
    let mut exp = n.mod_floor(&order);
    if exp.sign() == Sign::Minus {
        exp += order;
    }
    let (_, as_u8): (Sign, Vec<u8>) = exp.to_bytes_le();
    let mut scalar_array: [u8; 32] = [0; 32];
    scalar_array[..as_u8.len()].clone_from_slice(&as_u8[..]);
    let scalar = Scalar::from_bytes(&scalar_array).unwrap();
    let point: G1Affine = (G1Affine::generator() * scalar).into();
    point.to_compressed()
}

fn old_point_add(blobs: &[[u8; 48]]) -> [u8; 48] {
    let mut total: G1Projective = G1Projective::identity();
    for blob in blobs {
        let v = G1Affine::from_compressed(blob);
        if bool::from(v.is_some()) {
            total += &v.unwrap();
        }
    }
    let total: G1Affine = total.into();
    total.to_compressed()
}

fn setup_op_args(atoms: &[Vec<u8>]) -> (Arena, NodePtr) {
    let mut arena = Arena::new();
    let mut list = NodePtr::NIL;
    for blob in atoms.iter().rev() {
        let a = arena.new_atom(blob).expect("atom");
        list = arena.new_pair(a, list).expect("pair");
    }
    (arena, list)
}

fn bench_ops(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(0xBE7C_4A5E);
    let mut exponent = vec![0u8; 32];
    rng.fill(&mut exponent[..]);
    exponent[0] &= 0x7f; // positive

    let points: Vec<[u8; 48]> = (0..8)
        .map(|_| {
            let mut wide = [0u8; 64];
            rng.fill(&mut wide[..]);
            let scalar = Scalar::from_bytes_wide(&wide);
            let point: G1Affine = (G1Affine::generator() * scalar).into();
            point.to_compressed()
        })
        .collect();
    let point_atoms: Vec<Vec<u8>> = points.iter().map(|p| p.to_vec()).collect();
    let dialect = ChiaDialect::new(0);

    let mut group = c.benchmark_group("clvm_bls_ops");
    group.bench_function("pubkey_for_exp/old_bls12_381", |b| {
        b.iter(|| black_box(old_pubkey_for_exp(black_box(&exponent))));
    });
    group.bench_function("pubkey_for_exp/new_blst", |b| {
        b.iter_batched(
            || setup_op_args(std::slice::from_ref(&exponent)),
            |(arena, args)| {
                black_box(op_pubkey_for_exp(&arena, args, INFINITE_COST, &dialect)).expect("op ok")
            },
            BatchSize::SmallInput,
        );
    });
    for n in [2usize, 8] {
        group.bench_function(format!("point_add/{n}_points/old_bls12_381"), |b| {
            b.iter(|| black_box(old_point_add(black_box(&points[..n]))));
        });
        group.bench_function(format!("point_add/{n}_points/new_blst"), |b| {
            b.iter_batched(
                || setup_op_args(&point_atoms[..n]),
                |(arena, args)| {
                    black_box(op_point_add(&arena, args, INFINITE_COST, &dialect)).expect("op ok")
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_ops);
criterion_main!(benches);
