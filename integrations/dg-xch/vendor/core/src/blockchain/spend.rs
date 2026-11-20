use crate::blockchain::condition_with_args::MessageArgs;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::unsized_bytes::UnsizedBytes;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

// Spend-eligibility flags, set by the mempool condition parse only; consensus/block runs
// leave `flags` 0.

/// The spend may be deduplicated against an identical spend of the same coin in another mempool
/// item: no AGG_SIG_* conditions, no message conditions, and the coin amount does not exceed
/// its own outputs.
pub const ELIGIBLE_FOR_DEDUP: u32 = 1;

/// The spend carried at least one relative seconds/height condition.
pub const HAS_RELATIVE_CONDITION: u32 = 2;

/// The spend may be rebased onto a newer version of the same singleton: odd amount, no
/// parent-committing AGG_SIG conditions, no coin-id/parent-id/birth/relative/ephemeral
/// commitments (one ASSERT_MY_PARENT_ID as the second condition is the singleton top layer's
/// own and allowed), an output with the spend's own puzzle hash and amount, no
/// CREATE_COIN_ANNOUNCEMENT, no parent-mode messages, not referenced by an in-bundle
/// ASSERT_CONCURRENT_SPEND, and none of its outputs spent by the same bundle.
pub const ELIGIBLE_FOR_FF: u32 = 4;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct Spend {
    pub parent_id: Bytes32,
    pub coin_amount: u64,
    pub puzzle_hash: Bytes32,
    pub coin_id: Bytes32,
    pub height_relative: Option<u32>,
    pub seconds_relative: Option<u64>,
    pub before_height_relative: Option<u32>,
    pub before_seconds_relative: Option<u64>,
    pub birth_height: Option<u32>,
    pub birth_seconds: Option<u64>,
    pub create_coin: HashSet<NewCoin>,
    pub agg_sig_me: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_parent: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_puzzle: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_amount: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_puzzle_amount: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_parent_amount: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub agg_sig_parent_puzzle: Vec<(UnsizedBytes, UnsizedBytes)>,
    pub create_coin_announcements: Vec<UnsizedBytes>,
    pub assert_coin_announcements: Vec<Bytes32>,
    pub create_puzzle_announcements: Vec<UnsizedBytes>,
    pub assert_puzzle_announcements: Vec<Bytes32>,
    // ASSERT_CONCURRENT_SPEND (64): coin ids that must be spent in the same block
    pub assert_concurrent_spend: Vec<Bytes32>,
    // ASSERT_CONCURRENT_PUZZLE (65): puzzle hashes that must be spent in the same block
    pub assert_concurrent_puzzle: Vec<Bytes32>,
    // ASSERT_EPHEMERAL (76): this coin must have been created earlier in the same block
    pub assert_ephemeral: bool,
    // SEND_MESSAGE (66) emitted by this spend
    pub sent_messages: Vec<SpendMessage>,
    // RECEIVE_MESSAGE (67) emitted by this spend
    pub received_messages: Vec<SpendMessage>,
    pub flags: u32,
    // This spend's share of the bundle's condition cost (CREATE_COIN/AGG_SIG/etc.)
    #[serde(default)]
    pub condition_cost: u64,
    // The CLVM cost of running this spend's puzzle with its solution; filled on the per-spend
    // run paths, a whole-generator run cannot attribute it per spend
    #[serde(default)]
    pub execution_cost: u64,
}

// A CHIP-25 message emitted by a spend. `mode` packs the sender commitment in bits 3..6
// and the receiver commitment in bits 0..3; `args` carries the counterparty commitment
// (the destination for SEND_MESSAGE, the source for RECEIVE_MESSAGE).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct SpendMessage {
    pub mode: u8,
    pub message: Vec<u8>,
    pub args: MessageArgs,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct NewCoin {
    pub puzzle_hash: Bytes32,
    pub amount: u64,
    pub hint: Option<UnsizedBytes>,
}
impl Hash for NewCoin {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.puzzle_hash.hash(h);
        self.amount.hash(h);
    }
}
