// Modified by Chia Network for timelord-app; see integrations/dg-xch/VENDORING.md
use crate::clvm::arena::{Arena, NodePtr};
use crate::clvm::dialect::Dialect;
use crate::clvm::more_ops::BOOL_BASE_COST;
use log::info;
use std::io::Error;

/// Debug print for the `(all "$print$" ...)` convention: `args` are the operands AFTER the
/// print keyword, in order. Output format: `first: second, third`.
pub fn op_print<D: Dialect>(
    arena: &Arena,
    args: &[NodePtr],
    _max_cost: u64,
    _dialect: &D,
) -> Result<(u64, NodePtr), Error> {
    if args.is_empty() {
        Ok((BOOL_BASE_COST, NodePtr::NIL))
    } else {
        let mut buffer = String::new();
        match args.first() {
            Some(arg) => {
                buffer.extend(format!("{}:", arena.export(*arg)).chars());
                let mut cost = BOOL_BASE_COST * 2;
                let iter = args.iter().skip(1);
                for arg in iter {
                    cost += BOOL_BASE_COST;
                    buffer.extend(format!(" {},", arena.export(*arg)).chars());
                }
                buffer.remove(buffer.len() - 1);
                info!("CLVM DEBUG: {buffer}");
                Ok((cost, NodePtr::NIL))
            }
            None => Ok((BOOL_BASE_COST, NodePtr::NIL)),
        }
    }
}
