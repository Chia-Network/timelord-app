use blst::min_pk::{AggregateSignature, SecretKey, Signature};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dg_xch_core::blockchain::sized_bytes::{Bytes48, Bytes96};
use dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::clvm::bls_bindings::{aggregate_verify_signature, sign};
use dg_xch_core::consensus::block_generator::validate_block_aggregate_signature;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::traits::SizedBytes;
use dg_xch_core::utils::hash_256;
use std::hint::black_box;

// One block's AGG_SIG set: pubkeys, per-signature messages, and the aggregate.
// Keys/messages are derived deterministically (no network, no fixture) so the
// bench is reproducible; the verify path is the exact one the engine runs in
// validate_block_aggregate_signature.
struct BlockSigSet {
    public_keys: Vec<Bytes48>,
    messages: Vec<Vec<u8>>,
    aggregate: Signature,
}

fn build_sig_set(count: usize) -> BlockSigSet {
    let mut public_keys = Vec::with_capacity(count);
    let mut messages = Vec::with_capacity(count);
    let mut signatures = Vec::with_capacity(count);
    for i in 0..count {
        let ikm = hash_256((i as u64).to_le_bytes());
        let secret_key = SecretKey::key_gen(&ikm, &[]).unwrap();
        let mut message = Vec::with_capacity(96);
        message.extend_from_slice(&hash_256((i as u64).to_le_bytes()));
        message.extend_from_slice(&hash_256((i as u64 ^ u64::MAX).to_le_bytes()));
        message.extend_from_slice(&hash_256(
            (i as u64).wrapping_mul(0x9E37_79B9).to_le_bytes(),
        ));
        signatures.push(sign(&secret_key, &message));
        public_keys.push(Bytes48::from(secret_key.sk_to_pk()));
        messages.push(message);
    }
    let signature_refs = signatures.iter().collect::<Vec<&Signature>>();
    let aggregate = AggregateSignature::aggregate(&signature_refs, true)
        .unwrap()
        .to_signature();
    BlockSigSet {
        public_keys,
        messages,
        aggregate,
    }
}

// 532 = the AGG_SIG_ME count of real mainnet block 4671894 (a large tx block);
// 2048 stresses toward the max-block-cost signature ceiling.
fn bench_block_aggregate_verify(c: &mut Criterion) {
    let mut g = c.benchmark_group("block_aggregate_verify");
    for count in [532usize, 2048] {
        let set = build_sig_set(count);
        let message_refs = set
            .messages
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<&[u8]>>();
        g.throughput(Throughput::Elements(count as u64));
        g.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter(|| {
                let ok = aggregate_verify_signature(
                    black_box(&set.public_keys),
                    black_box(&message_refs),
                    black_box(&set.aggregate),
                );
                assert!(ok);
                black_box(ok);
            });
        });
    }
    g.finish();
}

// The engine entry point itself (`validate_block_aggregate_signature`), on AGG_SIG workloads
// with repeated keys — the shape real blocks have. Post-hard-fork mainnet census (heights 5,493,999–5,500,143):
// mean 20.6 pairs from 4.9 distinct keys per block; large blocks repeat keys even harder.
// Compare across git states with criterion baselines
// (`--save-baseline pre` on the old tree, `--baseline pre` on the new).
fn build_unsafe_conds(pairs: usize, distinct_keys: usize) -> (SpendBundleConditions, Bytes96) {
    let keys = (0..distinct_keys)
        .map(|i| {
            let ikm = hash_256((i as u64).to_le_bytes());
            SecretKey::key_gen(&ikm, &[]).unwrap()
        })
        .collect::<Vec<SecretKey>>();
    let mut conds = SpendBundleConditions::default();
    let mut signatures = Vec::with_capacity(pairs);
    for i in 0..pairs {
        let secret_key = &keys[i % distinct_keys];
        let message = hash_256((i as u64).to_le_bytes()).to_vec();
        signatures.push(sign(secret_key, &message));
        conds.agg_sig_unsafe.push((
            UnsizedBytes::new(secret_key.sk_to_pk().to_bytes().to_vec()),
            UnsizedBytes::new(message),
        ));
    }
    let signature_refs = signatures.iter().collect::<Vec<&Signature>>();
    let aggregate = AggregateSignature::aggregate(&signature_refs, true)
        .unwrap()
        .to_signature();
    (conds, Bytes96::parse(&aggregate.to_bytes()).unwrap())
}

fn bench_validate_block_aggregate_signature(c: &mut Criterion) {
    let mut g = c.benchmark_group("validate_block_aggregate_signature");
    // (pairs, distinct keys): the mean mainnet shape, a large repeated-key block, and the
    // no-repeat worst case (dedup map is pure overhead there — must stay in the noise).
    for (pairs, distinct) in [(21usize, 5usize), (532, 16), (532, 532)] {
        let (conds, aggregate) = build_unsafe_conds(pairs, distinct);
        g.throughput(Throughput::Elements(pairs as u64));
        g.bench_function(
            BenchmarkId::from_parameter(format!("{pairs}x{distinct}")),
            |b| {
                b.iter(|| {
                    validate_block_aggregate_signature(
                        black_box(&conds),
                        black_box(&aggregate),
                        black_box(&MAINNET),
                    )
                    .expect("verifies");
                });
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_block_aggregate_verify,
    bench_validate_block_aggregate_signature
);
criterion_main!(benches);
