// Sub-epoch-summary construction.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sub_epoch_summary::SubEpochSummary;
use crate::blockchain::unfinished_block::UnfinishedBlock;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::deficit::calculate_deficit;
use crate::consensus::difficulty_adjustment::{
    can_finish_sub_and_full_epoch, get_next_difficulty, get_next_sub_slot_iters,
    get_next_sub_slot_iters_and_difficulty, height_can_be_first_in_epoch,
};
use crate::consensus::missing;
use crate::consensus::pot_iterations::{calculate_ip_iters, calculate_sp_iters, is_overflow_block};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};

// dg_xch's SubEpochSummary is the mainnet-active 5-field form (no challenge_merkle_root); do not add the
// 6th field until it activates.
pub fn make_sub_epoch_summary(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
    blocks_included_height: u32,
    prev_prev_block: &BlockRecord,
    new_difficulty: Option<u64>,
    new_sub_slot_iters: Option<u64>,
) -> Result<SubEpochSummary, Error> {
    if prev_prev_block.height != blocks_included_height.wrapping_sub(2) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "prev_prev_block is not two heights below blocks_included_height",
        ));
    }
    // First sub-epoch: no previous summary to link, so it is genesis-anchored.
    if (u64::from(blocks_included_height) + u64::from(constants.max_sub_slot_blocks))
        / u64::from(constants.sub_epoch_blocks)
        <= 1
    {
        return Ok(SubEpochSummary {
            challenge_merkle_root: None,
            prev_subepoch_summary_hash: constants.genesis_challenge,
            reward_chain_hash: constants.genesis_challenge,
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        });
    }
    let mut curr = prev_prev_block;
    while curr.sub_epoch_summary_included.is_none() {
        curr = blocks
            .get(&curr.prev_hash)
            .ok_or_else(|| missing(curr.prev_hash))?;
    }
    let prev_ses_block = curr;
    let included = prev_ses_block
        .sub_epoch_summary_included
        .as_ref()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has no included summary",
            )
        })?;
    let reward_slot_hashes = prev_ses_block
        .finished_reward_slot_hashes
        .as_ref()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has no finished reward-slot hashes",
            )
        })?;
    Ok(SubEpochSummary {
        challenge_merkle_root: None,
        prev_subepoch_summary_hash: included.hash()?,
        reward_chain_hash: *reward_slot_hashes.last().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has empty finished reward-slot hashes",
            )
        })?,
        num_blocks_overflow: u8::try_from(prev_ses_block.height % constants.sub_epoch_blocks)
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "sub-epoch overflow block count exceeds u8",
                )
            })?,
        new_difficulty,
        new_sub_slot_iters,
    })
}

/// Returns the sub-epoch summary that the block AFTER `block` would include, if any — used
/// by the full node when it sends a farmed/received unfinished block on to timelords
/// (`NewUnfinishedBlockTimelord.sub_epoch_summary`). With `can_finish_soon` the block need
/// not be able to finish the epoch right now, only within `MAX_SUB_SLOT_BLOCKS`.
///
/// Returns `None` when: there is no previous block or the previous block is genesis; the
/// block just included a sub-epoch summary; or the block cannot finish the sub-epoch.
///
/// # Errors
/// Propagates store-walk gaps (a missing ancestor record) and the difficulty/SES-construction errors.
#[allow(clippy::too_many_lines)]
pub fn next_sub_epoch_summary(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
    required_iters: u64,
    block: &UnfinishedBlock,
    can_finish_soon: bool,
) -> Result<Option<SubEpochSummary>, Error> {
    let signage_point_index = block.reward_chain_block.signage_point_index;
    let prev_header_hash = block.foliage.prev_block_hash;
    let Some(prev_b) = blocks.get(&prev_header_hash) else {
        // prev not known yet yields None
        return Ok(None);
    };
    if prev_b.height == 0 {
        return Ok(None);
    }
    let num_finished_sub_slots = block.finished_sub_slots.len();
    let new_slot = num_finished_sub_slots > 0;

    if new_slot
        && block.finished_sub_slots[0]
            .challenge_chain
            .new_difficulty
            .is_some()
    {
        // We just included a sub-epoch summary.
        return Ok(None);
    }

    let sub_slot_iters =
        get_next_sub_slot_iters_and_difficulty(constants, new_slot, Some(prev_b), blocks)?.0;
    let overflow = is_overflow_block(constants, signage_point_index)?;

    if new_slot
        && block.finished_sub_slots[0]
            .challenge_chain
            .subepoch_summary_hash
            .is_some()
    {
        return Ok(None);
    }

    let deficit;
    let can_finish_se;
    let can_finish_epoch;
    if can_finish_soon {
        deficit = 0; // Assume that our deficit will go to zero soon.
        can_finish_se = true;
        if height_can_be_first_in_epoch(constants, prev_b.height + 2) {
            let mut epoch_ok = true;
            if (prev_b.height + 2) % constants.sub_epoch_blocks > 1 {
                let mut curr = prev_b;
                while curr.height % constants.sub_epoch_blocks > 0 {
                    if curr
                        .sub_epoch_summary_included
                        .as_ref()
                        .is_some_and(|s| s.new_difficulty.is_some())
                    {
                        epoch_ok = false;
                    }
                    curr = blocks
                        .get(&curr.prev_hash)
                        .ok_or_else(|| missing(curr.prev_hash))?;
                }
                if curr
                    .sub_epoch_summary_included
                    .as_ref()
                    .is_some_and(|s| s.new_difficulty.is_some())
                {
                    epoch_ok = false;
                }
            }
            can_finish_epoch = epoch_ok;
        } else {
            can_finish_epoch = height_can_be_first_in_epoch(
                constants,
                prev_b.height + constants.max_sub_slot_blocks + 2,
            );
        }
    } else {
        deficit = calculate_deficit(
            constants,
            prev_b.height + 1,
            Some(prev_b),
            overflow,
            num_finished_sub_slots,
        );
        let (se, epoch) = can_finish_sub_and_full_epoch(
            constants,
            blocks,
            prev_b.height + 1,
            prev_b.header_hash,
            deficit,
            false,
        )?;
        can_finish_se = se;
        can_finish_epoch = epoch;
    }

    // Can't finish sub-epoch, no summary.
    if !can_finish_se {
        return Ok(None);
    }

    let mut next_difficulty = None;
    let mut next_sub_slot_iters = None;
    if can_finish_epoch {
        let sp_iters = calculate_sp_iters(constants, sub_slot_iters, signage_point_index)?;
        let ip_iters = calculate_ip_iters(
            constants,
            sub_slot_iters,
            signage_point_index,
            required_iters,
        )?;
        // total_iters - ip_iters + sp_iters - (sub_slot_iters if overflow else 0)
        let total_iters = block.reward_chain_block.total_iters;
        let overflow_adj = if overflow {
            u128::from(sub_slot_iters)
        } else {
            0
        };
        let sp_total_iters =
            total_iters + u128::from(sp_iters) - u128::from(ip_iters) - overflow_adj;
        let prev_prev = blocks
            .get(&prev_b.prev_hash)
            .ok_or_else(|| missing(prev_b.prev_hash))?;
        let current_difficulty = u64::try_from(prev_b.weight - prev_prev.weight)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "difficulty exceeds u64"))?;
        next_difficulty = Some(get_next_difficulty(
            constants,
            blocks,
            prev_header_hash,
            prev_b.height + 1,
            current_difficulty,
            deficit,
            false,
            true,
            sp_total_iters,
        )?);
        next_sub_slot_iters = Some(get_next_sub_slot_iters(
            constants,
            blocks,
            prev_header_hash,
            prev_b.height + 1,
            sub_slot_iters,
            deficit,
            false,
            true,
            sp_total_iters,
        )?);
    }

    Ok(Some(make_sub_epoch_summary(
        constants,
        blocks,
        prev_b.height + 2,
        prev_b,
        next_difficulty,
        next_sub_slot_iters,
    )?))
}
