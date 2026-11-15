//! Tree-hash memoization for shared puzzle subtrees.

use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::sexp::SExp;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Node address: identity key for a live `SExp` node reached through the parsed
/// generator tree. Stable while the tree is alive (the cache's whole lifetime).
fn addr(node: &SExp) -> usize {
    std::ptr::from_ref(node) as usize
}

fn atom_hash(atom: &[u8]) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([1u8]);
    hasher.update(atom);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

fn pair_hash(first: &Bytes32, rest: &Bytes32) -> Bytes32 {
    let mut hasher = Sha256::new();
    hasher.update([2u8]);
    hasher.update(first);
    hasher.update(rest);
    let out: [u8; 32] = hasher.finalize().into();
    out.into()
}

enum TreeOp<'a> {
    Node(&'a SExp<'a>),
    Cons,
    ConsCache(usize),
}

/// Per-block tree-hash memoizer for pair nodes reached more than once.
#[derive(Default)]
pub struct TreeHashCache {
    /// visit counts, saturating at 2 — 2 means "shared, memoize its hash".
    counts: HashMap<usize, u8>,
    hashes: HashMap<usize, Bytes32>,
}

impl TreeHashCache {
    /// Mark shared pair nodes before hashing.
    pub fn visit_tree(&mut self, node: &SExp) {
        let mut stack: Vec<&SExp> = vec![node];
        while let Some(n) = stack.pop() {
            if let SExp::Pair(p) = n {
                let c = self.counts.entry(addr(n)).or_insert(0);
                if *c >= 1 {
                    // Revisited through sharing: mark memoize-worthy, don't re-descend.
                    *c = 2;
                    continue;
                }
                *c = 1;
                stack.push(p.first());
                stack.push(p.rest());
            }
        }
    }

    /// Compute the tree hash while memoizing shared subtrees.
    #[must_use]
    pub fn tree_hash(&mut self, node: &SExp) -> Bytes32 {
        let mut hashes: Vec<Bytes32> = Vec::new();
        let mut ops: Vec<TreeOp> = vec![TreeOp::Node(node)];
        while let Some(op) = ops.pop() {
            match op {
                TreeOp::Node(n) => match n {
                    SExp::Atom(a) => hashes.push(atom_hash(a.as_ref())),
                    SExp::Pair(p) => {
                        let key = addr(n);
                        if let Some(h) = self.hashes.get(&key) {
                            hashes.push(*h);
                        } else {
                            if self.counts.get(&key).copied().unwrap_or(0) >= 2 {
                                ops.push(TreeOp::ConsCache(key));
                            } else {
                                ops.push(TreeOp::Cons);
                            }
                            ops.push(TreeOp::Node(p.first()));
                            ops.push(TreeOp::Node(p.rest()));
                        }
                    }
                },
                TreeOp::Cons => {
                    let first = hashes.pop().expect("tree-hash op machine invariant");
                    let rest = hashes.pop().expect("tree-hash op machine invariant");
                    hashes.push(pair_hash(&first, &rest));
                }
                TreeOp::ConsCache(key) => {
                    let first = hashes.pop().expect("tree-hash op machine invariant");
                    let rest = hashes.pop().expect("tree-hash op machine invariant");
                    let h = pair_hash(&first, &rest);
                    self.hashes.insert(key, h);
                    hashes.push(h);
                }
            }
        }
        debug_assert_eq!(hashes.len(), 1);
        hashes.pop().expect("tree-hash op machine yields one hash")
    }
}
