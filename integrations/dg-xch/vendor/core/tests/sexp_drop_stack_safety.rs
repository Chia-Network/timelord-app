// An adversarial CLVM structure can decode to a deeply-nested owned `SExp`, and a recursive
// teardown would take one native stack frame per level and abort the process when it is dropped.
// These tests drop deep owned spines on a bounded 1 MiB worker stack to hold the iterative
// teardown in place.

use dg_xch_core::clvm::sexp::{AtomBuf, PairBuf, SExp};
use std::sync::Arc;

fn build_deep(depth: usize) -> SExp<'static> {
    let mut node = SExp::Atom(AtomBuf::new(Vec::new())); // nil tail
    for _ in 0..depth {
        node = SExp::Pair(PairBuf::Owned((
            Arc::new(SExp::Atom(AtomBuf::new(Vec::new()))), // first = ()
            Arc::new(node),                                 // rest = the deepening chain
        )));
    }
    node
}

#[test]
fn deep_owned_sexp_drops_without_stack_overflow() {
    // 500k levels overflow any reasonable stack under a per-level recursive drop.
    const DEPTH: usize = 500_000;
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let deep = build_deep(DEPTH);
            drop(deep);
            DEPTH
        })
        .expect("spawn worker");
    let n = handle
        .join()
        .expect("the deep owned SExp must drop without overflowing the stack");
    assert_eq!(n, DEPTH);
}

// The teardown must dismantle only solely-owned links and stop at any `Arc` shared with another
// holder, leaving that subtree intact.
#[test]
fn shared_arc_tail_survives_iterative_drop_of_sole_owned_prefix() {
    // A small, distinctly-shaped shared tail: (1 2 3), i.e. three-deep right-nested owned pairs.
    fn small_list() -> SExp<'static> {
        let mut node = SExp::Atom(AtomBuf::new(Vec::new()));
        for v in [3u8, 2, 1] {
            node = SExp::Pair(PairBuf::Owned((
                Arc::new(SExp::Atom(AtomBuf::new(vec![v]))),
                Arc::new(node),
            )));
        }
        node
    }
    fn owned_pair_depth(mut node: &SExp<'static>) -> usize {
        let mut d = 0;
        while let SExp::Pair(PairBuf::Owned((_, rest))) = node {
            d += 1;
            node = rest.as_ref();
        }
        d
    }

    let shared_tail: Arc<SExp<'static>> = Arc::new(small_list());
    let keep = shared_tail.clone(); // second, independent owner of the tail
    assert_eq!(Arc::strong_count(&keep), 2);
    let expected_depth = owned_pair_depth(&keep);
    assert_eq!(expected_depth, 3);

    // A sole-owned prefix deep enough to demand the iterative unwind, on a bounded stack.
    const PREFIX: usize = 200_000;
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            let mut node: Arc<SExp<'static>> = shared_tail; // strong_count now 2 (chain + keep)
            for _ in 0..PREFIX {
                node = Arc::new(SExp::Pair(PairBuf::Owned((
                    Arc::new(SExp::Atom(AtomBuf::new(Vec::new()))),
                    node,
                ))));
            }
            // The teardown walks down the prefix, reaches the shared tail, and must stop there.
            let prefix = Arc::try_unwrap(node).unwrap_or_else(|a| (*a).clone());
            drop(prefix);
        })
        .expect("spawn worker");
    handle
        .join()
        .expect("dropping the sole-owned prefix must not overflow the stack");

    // The shared tail is now solely owned by `keep` and must be byte-identical to a fresh build.
    assert_eq!(Arc::strong_count(&keep), 1);
    assert_eq!(owned_pair_depth(&keep), expected_depth);
    assert_eq!(*keep, small_list());
    assert_eq!(keep.tree_hash(), small_list().tree_hash());
}
