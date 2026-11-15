//! Operator results, described rather than allocated.
//!
//! Operators read through `&Arena` and cannot allocate; the runtime materializes what they
//! describe at a single site. Variants name the work so large results are written straight into
//! the arena with no intermediate buffer.

use crate::clvm::arena::{Arena, NodePtr};
use crate::clvm::sexp_ext::SExpNumber;
use crate::errors::ClvmError;

pub const MALLOC_COST_PER_BYTE: u64 = 10;

/// What an operator produced, described rather than allocated.
pub enum OpOut {
    /// An existing node returned unchanged (`f`, `r`, `i`, the ONE/NIL constants).
    Same(NodePtr),
    /// A pair of two existing nodes (`c`, `divmod`).
    Pair(NodePtr, NodePtr),
    /// A computed number; the arena encodes it. Priced after writing, since the malloc surcharge
    /// depends on the encoded length. Inline, not boxed: boxing cost a heap allocation per
    /// arithmetic operator.
    Number(SExpNumber),
    /// Fixed-size computed bytes: digests (32), G1 (48), G2 (96). Boxed — rare, and 96 bytes
    /// inline would widen every operator return. The operator prices it; the length is constant.
    Small(Box<([u8; 96], u8)>),
    /// Source atoms plus the total length; written straight into the heap, no intermediate buffer.
    Concat(Vec<NodePtr>, usize),
    /// A zero-copy sub-span of an existing atom.
    Substr(NodePtr, u32, u32),
    /// Two computed numbers as a pair (`divmod`), priced together after encoding.
    NumberPair(Box<(SExpNumber, SExpNumber)>),
}

impl OpOut {
    /// A freshly computed value referring to no existing node. Reclamation needs this: every
    /// other variant borrows a node that may live inside the region being rewound.
    #[must_use]
    pub fn is_self_contained(&self) -> bool {
        matches!(self, OpOut::Number(_) | OpOut::Small(_))
    }

    /// The 96-byte cap is the largest any operator produces (a G2 point).
    pub fn small(bytes: &[u8]) -> OpOut {
        let mut buf = [0u8; 96];
        buf[..bytes.len()].copy_from_slice(bytes);
        OpOut::Small(Box::new((buf, bytes.len() as u8)))
    }

    /// The only place in the operator path holding a mutable borrow.
    #[inline]
    pub fn materialize(self, arena: &mut Arena, cost: u64) -> Result<(u64, NodePtr), ClvmError> {
        match self {
            OpOut::Same(node) => Ok((cost, node)),
            OpOut::Pair(first, rest) => Ok((cost, arena.new_pair(first, rest)?)),
            OpOut::Number(n) => {
                let node = arena.new_number(&n)?;
                let len = arena
                    .atom_len(node)
                    .ok_or_else(|| ClvmError::ExpectedAtomGotPair(arena.display(node)))?;
                Ok((cost + len as u64 * MALLOC_COST_PER_BYTE, node))
            }
            OpOut::Small(b) => Ok((cost, arena.new_atom(&b.0[..b.1 as usize])?)),
            OpOut::Concat(nodes, total) => Ok((cost, arena.new_concat(total, &nodes)?)),
            OpOut::Substr(node, start, end) => Ok((cost, arena.new_substr(node, start, end)?)),
            OpOut::NumberPair(qr) => {
                let (q, r) = *qr;
                let q1 = arena.new_number(&q)?;
                let r1 = arena.new_number(&r)?;
                let c = (arena.atom_len(q1).unwrap_or(0) + arena.atom_len(r1).unwrap_or(0)) as u64
                    * MALLOC_COST_PER_BYTE;
                Ok((cost + c, arena.new_pair(q1, r1)?))
            }
        }
    }
}
