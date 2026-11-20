use crate::blockchain::class_group_element::ClassgroupElement;
use crate::blockchain::coin::Coin;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sub_epoch_summary::SubEpochSummary;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::pot_iterations::{calculate_ip_iters, calculate_sp_iters};
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct BlockRecord {
    pub header_hash: Bytes32,
    pub prev_hash: Bytes32,
    pub height: u32,
    pub weight: u128,
    pub total_iters: u128,
    pub signage_point_index: u8,
    pub challenge_vdf_output: ClassgroupElement,
    pub infused_challenge_vdf_output: Option<ClassgroupElement>,
    pub reward_infusion_new_challenge: Bytes32,
    pub challenge_block_info_hash: Bytes32,
    pub sub_slot_iters: u64,
    pub pool_puzzle_hash: Bytes32,
    pub farmer_puzzle_hash: Bytes32,
    pub required_iters: u64,
    pub deficit: u8,
    pub overflow: bool,
    pub prev_transaction_block_height: u32,
    pub timestamp: Option<u64>,
    pub prev_transaction_block_hash: Option<Bytes32>,
    pub fees: Option<u64>,
    pub reward_claims_incorporated: Option<Vec<Coin>>,
    pub finished_challenge_slot_hashes: Option<Vec<Bytes32>>,
    pub finished_infused_challenge_slot_hashes: Option<Vec<Bytes32>>,
    pub finished_reward_slot_hashes: Option<Vec<Bytes32>>,
    pub sub_epoch_summary_included: Option<SubEpochSummary>,
}

impl BlockRecord {
    #[must_use]
    pub fn first_in_sub_slot(&self) -> bool {
        self.finished_challenge_slot_hashes.is_some()
    }

    #[must_use]
    pub fn is_transaction_block(&self) -> bool {
        self.timestamp.is_some()
    }

    #[must_use]
    pub fn is_challenge_block(&self, min_blocks_per_challenge_block: u8) -> bool {
        self.deficit == min_blocks_per_challenge_block - 1
    }

    /// # Errors
    /// Returns an error if `calculate_ip_iters` rejects the record's iteration parameters.
    pub fn ip_iters(&self, constants: &ConsensusConstants) -> Result<u64, Error> {
        calculate_ip_iters(
            constants,
            self.sub_slot_iters,
            self.signage_point_index,
            self.required_iters,
        )
    }

    /// The signage-point iterations for this record.
    ///
    /// # Errors
    /// Returns an error if `calculate_sp_iters` rejects the record's iteration parameters.
    pub fn sp_iters(&self, constants: &ConsensusConstants) -> Result<u64, Error> {
        calculate_sp_iters(constants, self.sub_slot_iters, self.signage_point_index)
    }

    /// Total iterations at the infusion point of the
    /// sub-slot that contains this record (`total_iters - ip_iters`).
    ///
    /// # Errors
    /// Returns an error if `ip_iters` fails or the subtraction would underflow `u128`.
    pub fn ip_sub_slot_total_iters(&self, constants: &ConsensusConstants) -> Result<u128, Error> {
        self.total_iters
            .checked_sub(u128::from(self.ip_iters(constants)?))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "u128 underflow in ip_sub_slot_total_iters",
                )
            })
    }

    /// Total iterations at the start of the sub-slot
    /// that contains this record's signage point. Equal to `ip_sub_slot_total_iters`, less one full
    /// `sub_slot_iters` when this record is an overflow block.
    ///
    /// # Errors
    /// Returns an error if `ip_sub_slot_total_iters` fails or the overflow subtraction would underflow.
    pub fn sp_sub_slot_total_iters(&self, constants: &ConsensusConstants) -> Result<u128, Error> {
        let ret = self.ip_sub_slot_total_iters(constants)?;
        if self.overflow {
            ret.checked_sub(u128::from(self.sub_slot_iters))
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidData,
                        "u128 underflow in sp_sub_slot_total_iters",
                    )
                })
        } else {
            Ok(ret)
        }
    }

    /// Total iterations at this record's signage point
    /// (`sp_sub_slot_total_iters + sp_iters`).
    ///
    /// # Errors
    /// Returns an error if `sp_sub_slot_total_iters`/`sp_iters` fail or the addition would overflow `u128`.
    pub fn sp_total_iters(&self, constants: &ConsensusConstants) -> Result<u128, Error> {
        self.sp_sub_slot_total_iters(constants)?
            .checked_add(u128::from(self.sp_iters(constants)?))
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "u128 overflow in sp_total_iters"))
    }
}
