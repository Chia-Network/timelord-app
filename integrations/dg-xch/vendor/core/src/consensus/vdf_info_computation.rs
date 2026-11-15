// Reconstructing a block's signage-point VDF inputs and challenges.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::class_group_element::ClassgroupElement;
use crate::blockchain::end_of_subslot_bundle::EndOfSubSlotBundle;
use crate::blockchain::sized_bytes::Bytes32;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::{missing, rejected};
use std::collections::HashMap;
use std::io::Error;

// Returns (cc_vdf_challenge, rc_vdf_challenge, cc_vdf_input, rc_vdf_input, cc_vdf_iters,
// rc_vdf_iters);
// rc_vdf_input is always the identity element and cc_vdf_iters == rc_vdf_iters == sp_vdf_iters.
#[allow(clippy::type_complexity)]
pub fn get_signage_point_vdf_info(
    constants: &ConsensusConstants,
    finished_sub_slots: &[EndOfSubSlotBundle],
    overflow: bool,
    prev_b: Option<&BlockRecord>,
    blocks: &HashMap<Bytes32, BlockRecord>,
    sp_total_iters: u128,
    sp_iters: u64,
) -> Result<
    (
        Bytes32,
        Bytes32,
        ClassgroupElement,
        ClassgroupElement,
        u64,
        u64,
    ),
    Error,
> {
    let new_sub_slot = !finished_sub_slots.is_empty();
    let genesis_block = prev_b.is_none();
    let n = finished_sub_slots.len();

    let (cc_vdf_challenge, rc_vdf_challenge, cc_vdf_input, sp_vdf_iters): (
        Bytes32,
        Bytes32,
        ClassgroupElement,
        u64,
    );

    if new_sub_slot && !overflow {
        let last = &finished_sub_slots[n - 1];
        rc_vdf_challenge = last.reward_chain.hash()?;
        cc_vdf_challenge = last.challenge_chain.hash()?;
        sp_vdf_iters = sp_iters;
        cc_vdf_input = ClassgroupElement::get_default_element();
    } else if new_sub_slot && overflow && n > 1 {
        let prev = &finished_sub_slots[n - 2];
        rc_vdf_challenge = prev.reward_chain.hash()?;
        cc_vdf_challenge = prev.challenge_chain.hash()?;
        sp_vdf_iters = sp_iters;
        cc_vdf_input = ClassgroupElement::get_default_element();
    } else if genesis_block {
        rc_vdf_challenge = constants.genesis_challenge;
        cc_vdf_challenge = constants.genesis_challenge;
        sp_vdf_iters = sp_iters;
        cc_vdf_input = ClassgroupElement::get_default_element();
    } else if new_sub_slot && overflow && n == 1 {
        // Case 4.
        let prev = prev_b.ok_or_else(|| rejected("sp_vdf: prev_b"))?;
        let mut curr = prev;
        while !curr.first_in_sub_slot() && curr.total_iters > sp_total_iters {
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        if curr.total_iters < sp_total_iters {
            sp_vdf_iters = u64::try_from(sp_total_iters - curr.total_iters)
                .map_err(|_| rejected("sp_vdf: iters"))?;
            cc_vdf_input = curr.challenge_vdf_output;
            rc_vdf_challenge = curr.reward_infusion_new_challenge;
        } else {
            let hashes = curr
                .finished_reward_slot_hashes
                .as_ref()
                .ok_or_else(|| rejected("sp_vdf: no reward slot hashes"))?;
            sp_vdf_iters = sp_iters;
            cc_vdf_input = ClassgroupElement::get_default_element();
            rc_vdf_challenge = *hashes
                .last()
                .ok_or_else(|| rejected("sp_vdf: empty reward slot hashes"))?;
        }
        while !curr.first_in_sub_slot() {
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        let ch = curr
            .finished_challenge_slot_hashes
            .as_ref()
            .ok_or_else(|| rejected("sp_vdf: no challenge slot hashes"))?;
        cc_vdf_challenge = *ch
            .last()
            .ok_or_else(|| rejected("sp_vdf: empty challenge slot hashes"))?;
    } else if !new_sub_slot && overflow {
        // Case 5.
        let prev = prev_b.ok_or_else(|| rejected("sp_vdf: prev_b"))?;
        let mut curr = prev;
        let mut found_sub_slots: Vec<(Bytes32, Bytes32)> = Vec::new();
        if curr.first_in_sub_slot() {
            let ch = curr
                .finished_challenge_slot_hashes
                .as_ref()
                .ok_or_else(|| rejected("sp_vdf: no cc slot hashes"))?;
            let rw = curr
                .finished_reward_slot_hashes
                .as_ref()
                .ok_or_else(|| rejected("sp_vdf: no rc slot hashes"))?;
            found_sub_slots = ch.iter().copied().zip(rw.iter().copied()).rev().collect();
        }
        let mut sp_pre_sb: Option<&BlockRecord> = None;
        while found_sub_slots.len() < 2 && curr.height > 0 {
            if sp_pre_sb.is_none() && curr.total_iters < sp_total_iters {
                sp_pre_sb = Some(curr);
            }
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
            if curr.first_in_sub_slot() {
                let ch = curr
                    .finished_challenge_slot_hashes
                    .as_ref()
                    .ok_or_else(|| rejected("sp_vdf: no cc slot hashes"))?;
                let rw = curr
                    .finished_reward_slot_hashes
                    .as_ref()
                    .ok_or_else(|| rejected("sp_vdf: no rc slot hashes"))?;
                found_sub_slots.extend(ch.iter().copied().zip(rw.iter().copied()).rev());
            }
        }
        if sp_pre_sb.is_none() && curr.total_iters < sp_total_iters {
            sp_pre_sb = Some(curr);
        }
        if found_sub_slots.len() < 2 {
            return Err(rejected("sp_vdf: <2 found_sub_slots"));
        }
        if let Some(pre) = sp_pre_sb {
            sp_vdf_iters = u64::try_from(sp_total_iters - pre.total_iters)
                .map_err(|_| rejected("sp_vdf: iters"))?;
            cc_vdf_input = pre.challenge_vdf_output;
            rc_vdf_challenge = pre.reward_infusion_new_challenge;
        } else {
            sp_vdf_iters = sp_iters;
            cc_vdf_input = ClassgroupElement::get_default_element();
            rc_vdf_challenge = found_sub_slots[1].1;
        }
        cc_vdf_challenge = found_sub_slots[1].0;
    } else if !new_sub_slot && !overflow {
        // Case 6.
        let prev = prev_b.ok_or_else(|| rejected("sp_vdf: prev_b"))?;
        let mut curr = prev;
        while !curr.first_in_sub_slot() && curr.total_iters > sp_total_iters {
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        if curr.total_iters < sp_total_iters {
            sp_vdf_iters = u64::try_from(sp_total_iters - curr.total_iters)
                .map_err(|_| rejected("sp_vdf: iters"))?;
            cc_vdf_input = curr.challenge_vdf_output;
            rc_vdf_challenge = curr.reward_infusion_new_challenge;
        } else {
            let hashes = curr
                .finished_reward_slot_hashes
                .as_ref()
                .ok_or_else(|| rejected("sp_vdf: no reward slot hashes"))?;
            sp_vdf_iters = sp_iters;
            cc_vdf_input = ClassgroupElement::get_default_element();
            rc_vdf_challenge = *hashes
                .last()
                .ok_or_else(|| rejected("sp_vdf: empty reward slot hashes"))?;
        }
        while !curr.first_in_sub_slot() {
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        let ch = curr
            .finished_challenge_slot_hashes
            .as_ref()
            .ok_or_else(|| rejected("sp_vdf: no challenge slot hashes"))?;
        cc_vdf_challenge = *ch
            .last()
            .ok_or_else(|| rejected("sp_vdf: empty challenge slot hashes"))?;
    } else {
        return Err(rejected("sp_vdf: unreachable case"));
    }

    Ok((
        cc_vdf_challenge,
        rc_vdf_challenge,
        cc_vdf_input,
        ClassgroupElement::get_default_element(),
        sp_vdf_iters,
        sp_vdf_iters,
    ))
}
