use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sync::Sync;
use crate::formatting::parse_u128;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, Copy, Clone, Serialize, Deserialize, Debug, Default)]
pub struct MinMempoolFees {
    pub cost_5000000: f64,
}

impl PartialEq for MinMempoolFees {
    fn eq(&self, other: &Self) -> bool {
        if self.cost_5000000.is_infinite() && other.cost_5000000.is_infinite() {
            true
        } else if self.cost_5000000.is_infinite() || other.cost_5000000.is_infinite() {
            false
        } else if self.cost_5000000.is_nan() && other.cost_5000000.is_nan() {
            true
        } else if self.cost_5000000.is_nan() || other.cost_5000000.is_nan() {
            false
        } else {
            self.cost_5000000 == other.cost_5000000
        }
    }
}

impl Eq for MinMempoolFees {}
#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
pub struct BlockchainState {
    pub peak: Option<BlockRecord>,
    pub genesis_challenge_initialized: bool,
    pub sync: Sync,
    pub difficulty: u64,
    pub sub_slot_iters: u64,
    #[serde(deserialize_with = "parse_u128")]
    pub space: u128,
    pub mempool_size: u64,
    pub mempool_cost: u64,
    pub mempool_min_fees: MinMempoolFees,
    pub mempool_max_total_cost: u64,
    pub block_max_cost: u64,
    pub node_id: Bytes32,
}
