// Invariants of the non-allocating operator protocol, per result shape.
//
// Operators take `&Arena` and cannot allocate; each returns an `OpOut` the runtime materializes
// at one site. The equivalence this file originally checked — prototype against the shipped
// mutable operators — no longer exists to state, since the protocol IS the shipped path (byte
// and cost goldens live in `clvm_op_vectors`). What still has to hold, per shape:
//
//   - `f`      returns an existing node: no storage may move and no malloc surcharge applies;
//   - `c`      builds exactly one pair from nodes it never inspects, at CONS_COST;
//   - `+`      prices its result by the ENCODED length, which does not exist until materialize;
//   - `concat` writes straight into the heap — exactly the output bytes, no intermediate copy.
//
// Inputs come from a seeded PRNG biased toward the encodings that have historically broken
// things (empty atoms, sign-bit edges, non-canonical zeros, multi-hundred-byte payloads).

use dg_xch_core::clvm::arena::{Arena, NodePtr};
use dg_xch_core::clvm::dialect::ChiaDialect;
use dg_xch_core::clvm::pure_ops::{MALLOC_COST_PER_BYTE, OpOut};
use dg_xch_core::clvm::{core_ops, more_ops};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Atom byte strings biased toward the encodings that have historically broken things.
fn gen_atom(rng: &mut Rng) -> Vec<u8> {
    match rng.below(10) {
        0 => vec![],
        1 => vec![0],
        2 => vec![0x80],
        3 => vec![0xff],
        4 => vec![0, 0x80],
        5 => vec![0x03, 0xff, 0xff, 0xff],
        6 => (0..rng.below(4) + 1).map(|_| rng.next() as u8).collect(),
        // Large atoms: where an extra copy in the protocol would actually cost something.
        _ => (0..rng.below(600) + 1).map(|_| rng.next() as u8).collect(),
    }
}

fn list_of(arena: &mut Arena, items: &[NodePtr]) -> NodePtr {
    let mut out = NodePtr::NIL;
    for item in items.iter().rev() {
        out = arena.new_pair(*item, out).expect("pair");
    }
    out
}

#[test]
fn first_moves_no_storage_and_adds_no_surcharge() {
    let dialect = ChiaDialect::new(0);
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut arena = Arena::new();
        let a = arena.new_atom(&gen_atom(&mut rng)).expect("atom");
        let b = arena.new_atom(&gen_atom(&mut rng)).expect("atom");
        let inner = arena.new_pair(a, b).expect("pair");
        let args = list_of(&mut arena, &[inner]);

        let stored = (
            arena.stored_atom_count(),
            arena.stored_pair_count(),
            arena.stored_heap_bytes(),
        );
        let (cost, out) = core_ops::op_first(&arena, args, u64::MAX, &dialect).expect("f");
        assert!(
            matches!(out, OpOut::Same(n) if n == a),
            "seed {seed}: f returns the node itself"
        );
        let (total, node) = out.materialize(&mut arena, cost).expect("materialize");
        assert_eq!(node, a, "seed {seed}");
        assert_eq!(
            total, cost,
            "seed {seed}: Same must add no malloc surcharge"
        );
        assert_eq!(
            (
                arena.stored_atom_count(),
                arena.stored_pair_count(),
                arena.stored_heap_bytes(),
            ),
            stored,
            "seed {seed}: f moved storage"
        );
    }
}

#[test]
fn cons_builds_exactly_one_pair_without_reading_its_nodes() {
    let dialect = ChiaDialect::new(0);
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut arena = Arena::new();
        let a = arena.new_atom(&gen_atom(&mut rng)).expect("atom");
        let b = arena.new_atom(&gen_atom(&mut rng)).expect("atom");
        let args = list_of(&mut arena, &[a, b]);

        let pairs = arena.stored_pair_count();
        let heap = arena.stored_heap_bytes();
        let (cost, out) = core_ops::op_cons(&arena, args, u64::MAX, &dialect).expect("c");
        assert!(
            matches!(out, OpOut::Pair(x, y) if x == a && y == b),
            "seed {seed}"
        );
        let (total, node) = out.materialize(&mut arena, cost).expect("materialize");
        assert_eq!(total, cost, "seed {seed}: Pair carries no malloc surcharge");
        assert_eq!(
            arena.stored_pair_count(),
            pairs + 1,
            "seed {seed}: exactly one pair"
        );
        assert_eq!(
            arena.stored_heap_bytes(),
            heap,
            "seed {seed}: no heap bytes for a cons"
        );
        let (first, rest) = match arena.node_kind(node) {
            dg_xch_core::clvm::arena::NodeKind::Pair(f, r) => (f, r),
            dg_xch_core::clvm::arena::NodeKind::Atom => panic!("seed {seed}: cons made an atom"),
        };
        assert_eq!((first, rest), (a, b), "seed {seed}");
    }
}

#[test]
fn add_prices_by_the_encoded_length_only_at_materialize() {
    let dialect = ChiaDialect::new(0);
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut arena = Arena::new();
        let mut items = Vec::new();
        for _ in 0..rng.below(4) + 1 {
            // Canonical integers only: `+` rejects the deliberately broken encodings.
            let n = (rng.next() as i64) >> rng.below(48);
            let num = dg_xch_core::clvm::sexp_ext::SExpNumber::I128(i128::from(n));
            items.push(arena.new_number(&num).expect("number"));
        }
        let args = list_of(&mut arena, &items);

        let (cost, out) = more_ops::op_add(&arena, args, u64::MAX, &dialect).expect("+");
        assert!(
            matches!(out, OpOut::Number(_)),
            "seed {seed}: + describes a number"
        );
        let (total, node) = out.materialize(&mut arena, cost).expect("materialize");
        let encoded = arena.atom_len(node).expect("atom result") as u64;
        assert_eq!(
            total,
            cost + encoded * MALLOC_COST_PER_BYTE,
            "seed {seed}: surcharge must equal the encoded length"
        );
    }
}

#[test]
fn concat_writes_exactly_the_output_bytes() {
    let dialect = ChiaDialect::new(0);
    for seed in 0..300u64 {
        let mut rng = Rng::new(seed);
        let mut arena = Arena::new();
        let mut blobs = Vec::new();
        let mut items = Vec::new();
        for _ in 0..rng.below(5) + 1 {
            let blob = gen_atom(&mut rng);
            items.push(arena.new_atom(&blob).expect("atom"));
            blobs.push(blob);
        }
        let args = list_of(&mut arena, &items);
        let expected: Vec<u8> = blobs.concat();

        let heap = arena.stored_heap_bytes();
        let logical_heap = arena.counters().2;
        let (cost, out) = more_ops::op_concat(&arena, args, u64::MAX, &dialect).expect("concat");
        match &out {
            OpOut::Concat(nodes, total) => {
                assert_eq!(*total, expected.len(), "seed {seed}: total must match");
                assert_eq!(nodes.len(), items.len(), "seed {seed}");
            }
            // A single-argument concat may legitimately return the node unchanged.
            OpOut::Same(_) => {}
            _ => panic!("seed {seed}: unexpected concat shape"),
        }
        let single = matches!(out, OpOut::Same(_));
        let (_, node) = out.materialize(&mut arena, cost).expect("materialize");
        let got = arena.atom(node).expect("atom result");
        assert_eq!(
            got.as_ref(),
            expected.as_slice(),
            "seed {seed}: concat bytes"
        );
        if !single {
            // Interning may satisfy the write from an existing span, so stored growth is AT
            // MOST the output; the logical count grows by exactly the output either way.
            assert!(
                arena.stored_heap_bytes() <= heap + expected.len(),
                "seed {seed}: heap grew past the output — an intermediate copy"
            );
            assert_eq!(
                arena.counters().2,
                logical_heap + expected.len(),
                "seed {seed}: logical heap must grow by exactly the output"
            );
        }
    }
}
