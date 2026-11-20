use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ChallengeChainSubSlot {
    pub challenge_chain_end_of_slot_vdf: VdfInfo,
    pub infused_challenge_chain_sub_slot_hash: Option<Bytes32>,
    pub subepoch_summary_hash: Option<Bytes32>,
    pub new_sub_slot_iters: Option<u64>,
    pub new_difficulty: Option<u64>,
}

impl ChallengeChainSubSlot {
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
