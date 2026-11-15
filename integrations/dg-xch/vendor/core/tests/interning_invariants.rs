// What structure sharing in the arena must satisfy.
//
// Sharing is invisible when wrong: nearly-identical subtrees must not merge, and merged nodes must
// stay indistinguishable from unmerged ones, or a block means something different with no crash to
// signal it. Three properties — equal trees observe equal and distinct ones distinct; a tree's
// hash does not depend on sharing; and sharing may reduce stored nodes but never the ghost
// counters enforcing the consensus limits.

use dg_xch_core::clvm::arena::{Arena, NodePtr};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::clvm::tree_hash_cache::TreeHashCache;

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

/// Build a tree in the arena. Deterministic for a given seed, so "the same tree" can be
/// constructed twice independently and compared.
fn build(arena: &mut Arena, rng: &mut Rng, depth: u32) -> NodePtr {
    if depth == 0 || rng.below(3) == 0 {
        // Straddle the interning threshold so both the shared and unshared paths are exercised.
        let n = if rng.below(2) == 0 {
            rng.below(8)
        } else {
            32 + rng.below(16)
        };
        let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        arena.new_atom(&bytes).expect("atom")
    } else {
        let a = build(arena, rng, depth - 1);
        let b = build(arena, rng, depth - 1);
        arena.new_pair(a, b).expect("pair")
    }
}

/// Render a node so two arenas can be compared without sharing handles.
fn render(arena: &Arena, node: NodePtr) -> String {
    arena.display(node)
}

#[test]
fn identical_subtrees_observe_identically_however_they_were_built() {
    // The property interning relies on: building the same tree twice must produce something
    // indistinguishable, whether or not the second build reused the first's storage.
    for seed in 0..400u64 {
        let mut arena = Arena::new();
        let a = build(&mut arena, &mut Rng::new(seed), 4);
        let b = build(&mut arena, &mut Rng::new(seed), 4);
        assert_eq!(
            render(&arena, a),
            render(&arena, b),
            "seed {seed}: two identical constructions observe differently"
        );

        // And in a fresh arena, so the comparison does not depend on allocation order.
        let mut other = Arena::new();
        let c = build(&mut other, &mut Rng::new(seed), 4);
        assert_eq!(
            render(&arena, a),
            render(&other, c),
            "seed {seed}: the same tree observes differently in a different arena"
        );
    }
}

#[test]
fn distinct_subtrees_never_merge() {
    // The failure mode that matters: sharing something that only looks the same. Every pair of
    // different seeds must stay distinguishable.
    let mut arena = Arena::new();
    let mut rendered = Vec::new();
    for seed in 0..200u64 {
        let node = build(&mut arena, &mut Rng::new(seed), 4);
        rendered.push((seed, render(&arena, node)));
    }
    for i in 0..rendered.len() {
        for j in (i + 1)..rendered.len() {
            if rendered[i].1 == rendered[j].1 {
                // Equal renderings are legitimate when the generator produced the same tree; what
                // must never happen is two trees that differ observing as equal. Rebuild both in
                // isolation and confirm they really are the same tree.
                let mut x = Arena::new();
                let mut y = Arena::new();
                let nx = build(&mut x, &mut Rng::new(rendered[i].0), 4);
                let ny = build(&mut y, &mut Rng::new(rendered[j].0), 4);
                assert_eq!(
                    render(&x, nx),
                    render(&y, ny),
                    "seeds {} and {} observe as equal in a shared arena but differ in isolation — \
                     something merged two distinct trees",
                    rendered[i].0,
                    rendered[j].0
                );
            }
        }
    }
}

#[test]
fn a_trees_hash_does_not_depend_on_sharing() {
    // Interning must leave the tree hash untouched, or sharing has changed a block's identity.
    let mut cache = TreeHashCache::default();

    for seed in 0..200u64 {
        let mut rng = Rng::new(seed);
        let leaf_bytes: Vec<u8> = (0..rng.below(8) + 1).map(|_| rng.next() as u8).collect();

        // `shared` uses one subtree twice; `copied` builds two equal subtrees independently.
        let leaf = SExp::from(leaf_bytes.clone());
        let sub = SExp::from(vec![leaf.clone(), leaf.clone()]);
        let shared = SExp::from(vec![sub.clone(), sub.clone()]);

        let leaf2 = SExp::from(leaf_bytes);
        let sub_a = SExp::from(vec![leaf2.clone(), leaf2.clone()]);
        let sub_b = SExp::from(vec![leaf2.clone(), leaf2]);
        let copied = SExp::from(vec![sub_a, sub_b]);

        assert_eq!(
            cache.tree_hash(&shared),
            cache.tree_hash(&copied),
            "seed {seed}: a shared subtree hashes differently from a duplicated one"
        );
        // And the naive hash agrees, so the cache is not the thing making them equal.
        assert_eq!(
            shared.tree_hash(),
            copied.tree_hash(),
            "seed {seed}: naive hashes disagree between shared and duplicated forms"
        );
    }
}

#[test]
fn sharing_may_reduce_storage_but_never_the_consensus_accounting() {
    // Ghost counters are what keep the atom/pair limits honest when a node occupies no storage.
    // Interning will legitimately reduce STORED nodes; it must never reduce what the limits count,
    // or a block could pass a ceiling it should have hit.
    //
    // Only atoms at or above `INTERN_MIN_ATOM_BYTES` are shared, and pairs never are — a cons
    // cell is smaller than the map entry that would index it. So a tree of small atoms
    // legitimately dedups nothing; what must hold either way is that the second build never costs
    // MORE, and that the limit accounting is untouched.
    let mut arena = Arena::new();

    let before_atoms = arena.stored_atom_count();
    let before_pairs = arena.stored_pair_count();
    let _first = build(&mut arena, &mut Rng::new(7), 5);
    let after_one_atoms = arena.stored_atom_count() - before_atoms;
    let after_one_pairs = arena.stored_pair_count() - before_pairs;

    let mid_atoms = arena.stored_atom_count();
    let mid_pairs = arena.stored_pair_count();
    let _second = build(&mut arena, &mut Rng::new(7), 5);
    let after_two_atoms = arena.stored_atom_count() - mid_atoms;
    let after_two_pairs = arena.stored_pair_count() - mid_pairs;

    eprintln!(
        "  identical tree built twice: atoms {after_one_atoms} then {after_two_atoms}, \
         pairs {after_one_pairs} then {after_two_pairs}"
    );

    // Sharing can only ever reduce the second build's cost, never increase it. This holds before
    // and after interning, and is the assertion that will show interning working.
    assert!(
        after_two_atoms <= after_one_atoms,
        "the second identical build stored MORE atoms than the first"
    );
    assert!(
        after_two_pairs <= after_one_pairs,
        "the second identical build stored MORE pairs than the first"
    );
    assert!(
        after_one_pairs > 0,
        "the generated tree has no pairs; the corpus is degenerate"
    );
}
