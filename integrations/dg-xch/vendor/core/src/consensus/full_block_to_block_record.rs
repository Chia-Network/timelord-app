// Build a BlockRecord from a validated header block.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::challenge_block_info::ChallengeBlockInfo;
use crate::blockchain::header_block::HeaderBlock;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sub_epoch_summary::SubEpochSummary;
use crate::consensus::constants::ConsensusConstants;
use std::io::Error;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn header_block_to_sub_block_record(
    constants: &ConsensusConstants,
    required_iters: u64,
    block: &HeaderBlock,
    sub_slot_iters: u64,
    overflow: bool,
    deficit: u8,
    prev_transaction_block_height: u32,
    ses: Option<SubEpochSummary>,
) -> Result<BlockRecord, Error> {
    let rcb = &block.reward_chain_block;
    let cbi = ChallengeBlockInfo {
        proof_of_space: rcb.proof_of_space.clone(),
        challenge_chain_sp_vdf: rcb.challenge_chain_sp_vdf,
        challenge_chain_sp_signature: rcb.challenge_chain_sp_signature,
        challenge_chain_ip_vdf: rcb.challenge_chain_ip_vdf,
    };
    let icc_output = rcb.infused_challenge_chain_ip_vdf.map(|v| v.output);

    let (fcsh, frsh, ficsh): (
        Option<Vec<Bytes32>>,
        Option<Vec<Bytes32>>,
        Option<Vec<Bytes32>>,
    ) = if !block.finished_sub_slots.is_empty() {
        let mut cc = Vec::with_capacity(block.finished_sub_slots.len());
        let mut rw = Vec::with_capacity(block.finished_sub_slots.len());
        let mut icc = Vec::new();
        for ss in &block.finished_sub_slots {
            cc.push(ss.challenge_chain.hash()?);
            rw.push(ss.reward_chain.hash()?);
            if let Some(icc_ss) = &ss.infused_challenge_chain {
                icc.push(icc_ss.hash()?);
            }
        }
        (Some(cc), Some(rw), Some(icc))
    } else if block.height() == 0 {
        (
            Some(vec![constants.genesis_challenge]),
            Some(vec![constants.genesis_challenge]),
            None,
        )
    } else {
        (None, None, None)
    };

    let (timestamp, prev_tx_hash) = match &block.foliage_transaction_block {
        Some(ftb) => (Some(ftb.timestamp), Some(ftb.prev_transaction_block_hash)),
        None => (None, None),
    };
    let (fees, reward_claims) = match &block.transactions_info {
        Some(ti) => (Some(ti.fees), Some(ti.reward_claims_incorporated.clone())),
        None => (None, None),
    };

    Ok(BlockRecord {
        header_hash: block.header_hash()?,
        prev_hash: block.prev_header_hash(),
        height: block.height(),
        weight: block.weight(),
        total_iters: block.total_iters(),
        signage_point_index: rcb.signage_point_index,
        challenge_vdf_output: rcb.challenge_chain_ip_vdf.output,
        infused_challenge_vdf_output: icc_output,
        reward_infusion_new_challenge: rcb.hash()?,
        challenge_block_info_hash: cbi.hash()?,
        sub_slot_iters,
        pool_puzzle_hash: block.foliage.foliage_block_data.pool_target.puzzle_hash,
        farmer_puzzle_hash: block.foliage.foliage_block_data.farmer_reward_puzzle_hash,
        required_iters,
        deficit,
        overflow,
        prev_transaction_block_height,
        timestamp,
        prev_transaction_block_hash: prev_tx_hash,
        fees,
        reward_claims_incorporated: reward_claims,
        finished_challenge_slot_hashes: fcsh,
        finished_infused_challenge_slot_hashes: ficsh,
        finished_reward_slot_hashes: frsh,
        sub_epoch_summary_included: ses,
    })
}
