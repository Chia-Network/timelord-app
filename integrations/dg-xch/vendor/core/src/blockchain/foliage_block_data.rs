use crate::blockchain::pool_target::PoolTarget;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FoliageBlockData {
    pub unfinished_reward_block_hash: Bytes32,
    pub pool_target: PoolTarget,
    pub pool_signature: Option<Bytes96>,
    pub farmer_reward_puzzle_hash: Bytes32,
    pub extension_data: Bytes32,
}

impl FoliageBlockData {
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
