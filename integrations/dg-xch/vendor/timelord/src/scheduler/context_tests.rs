// Added by Chia Network for timelord-app; see integrations/dg-xch/VENDORING.md
use super::*;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

fn bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn a_new_peak_retries_deferred_candidates_without_stopping_other_work() {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/chia_hf2.json")).unwrap();
    let sequence = data["peer_sequence"].as_array().unwrap();
    let raw = bytes(sequence[0]["bytes"].as_str().unwrap());
    let peak = NewPeakTimelord::from_bytes(
        &mut Cursor::new(raw.as_slice()),
        ChiaProtocolVersion::default(),
    )
    .unwrap();
    let raw = bytes(sequence[1]["bytes"].as_str().unwrap());
    let candidate = NewUnfinishedBlockTimelord::from_bytes(
        &mut Cursor::new(raw.as_slice()),
        ChiaProtocolVersion::default(),
    )
    .unwrap();
    let mut c = dg_xch_core::consensus::constants::MAINNET;
    c.min_plot_size = 18;
    c.hard_fork_height = 0;
    c.epoch_blocks = 768;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_plot_strength = 9;
    c.difficulty_constant_factor = 10052721566054;
    let mut scheduler = Scheduler::new(c, false, None).unwrap();
    let hash = candidate.reward_chain_block.hash().unwrap();
    scheduler
        .deferred
        .insert(hash.const_bytes(), candidate.clone());
    let mut invalid = candidate;
    invalid.reward_chain_block.proof_of_space =
        serde_json::from_value(data["rows"][0]["reward_block"]["proof_of_space"].clone()).unwrap();
    invalid.reward_chain_block.proof_of_space.proof =
        serde_json::from_value(serde_json::json!("0x00")).unwrap();
    scheduler.deferred.insert([0; 32], invalid);
    assert!(scheduler.set_peak(peak.clone()).unwrap());
    assert!(scheduler.deferred.is_empty());
    assert_eq!(scheduler.candidates.len(), 1);
    let outcomes = scheduler.take_retry_outcomes();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().any(|(key, outcome)| {
        *key == hash && matches!(outcome, Ok(Admission::Admitted(admitted)) if *admitted == hash)
    }));
    assert!(
        outcomes
            .iter()
            .any(|(_, outcome)| outcome.as_ref().is_err()),
        "retry errors must be retained for observers"
    );
    assert!(scheduler.next_plan().unwrap().is_some());
    // A noncontiguous, non-transaction peak must lose transaction context rather
    // than inventing a previous transaction height from the candidate height.
    let mut later = peak;
    later.reward_chain_block.height += 2;
    later.reward_chain_block.is_transaction_block = false;
    scheduler
        .retry_outcomes
        .push((Bytes32::default(), Ok(Admission::Stale)));
    assert!(scheduler.set_peak(later).unwrap());
    assert_eq!(scheduler.transaction_height, None);
    assert!(
        scheduler.retry_outcomes.is_empty(),
        "a peak replaces the previous peak's retry outcomes"
    );
    assert!(scheduler.next_plan().unwrap().is_some());
}

#[test]
fn a_noncontiguous_peak_only_keeps_an_ancestor_transaction_lower_bound() {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/chia_hf2.json")).unwrap();
    let sequence = data["peer_sequence"].as_array().unwrap();
    let raw = bytes(sequence[0]["bytes"].as_str().unwrap());
    let peak = NewPeakTimelord::from_bytes(
        &mut Cursor::new(raw.as_slice()),
        ChiaProtocolVersion::default(),
    )
    .unwrap();
    let mut c = dg_xch_core::consensus::constants::MAINNET;
    c.min_plot_size = 18;
    c.hard_fork_height = 0;
    c.epoch_blocks = 768;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_plot_strength = 9;
    c.difficulty_constant_factor = 10052721566054;
    let mut scheduler = Scheduler::new(c, false, None).unwrap();
    assert!(scheduler.set_peak(peak.clone()).unwrap());

    let ancestor = peak.previous_reward_challenges[0].0;
    scheduler.last_transaction = Some((10_000, ancestor));
    scheduler.transaction_height = None;
    let mut contiguous_history = peak.clone();
    contiguous_history.reward_chain_block.height += 2;
    contiguous_history.reward_chain_block.is_transaction_block = false;
    assert!(scheduler.set_peak(contiguous_history).unwrap());
    assert_eq!(scheduler.last_transaction, Some((10_000, ancestor)));

    let unrelated = Bytes32::from([0xAB; 32]);
    scheduler.last_transaction = Some((10_001, unrelated));
    scheduler.transaction_height = None;
    let mut unrelated_history = peak;
    unrelated_history.reward_chain_block.height += 4;
    unrelated_history.reward_chain_block.is_transaction_block = false;
    assert!(scheduler.set_peak(unrelated_history).unwrap());
    assert_eq!(
        scheduler.last_transaction, None,
        "a non-ancestor transaction block cannot bound the next candidate"
    );
}

#[test]
fn full_deferral_queue_reports_the_shed_candidate() {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/chia_hf2.json")).unwrap();
    let sequence = data["peer_sequence"].as_array().unwrap();
    let raw = bytes(sequence[0]["bytes"].as_str().unwrap());
    let peak = NewPeakTimelord::from_bytes(
        &mut Cursor::new(raw.as_slice()),
        ChiaProtocolVersion::default(),
    )
    .unwrap();
    let raw = bytes(data["rows"][0]["unfinished_bytes"].as_str().unwrap());
    let candidate = NewUnfinishedBlockTimelord::from_bytes(
        &mut Cursor::new(raw.as_slice()),
        ChiaProtocolVersion::default(),
    )
    .unwrap();
    let mut c = dg_xch_core::consensus::constants::MAINNET;
    c.min_plot_size = 18;
    c.hard_fork_height = 0;
    c.epoch_blocks = 768;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_plot_strength = 9;
    c.difficulty_constant_factor = 10052721566054;
    c.min_plot_strength = 1;
    c.max_effective_plot_filter_bits = 7;
    c.plot_filter_v2_relative_height =
        dg_xch_core::consensus::filter_schedule::FilterSchedule::try_from_slice(&[
            33000, 30000, 27000, 24000, 21000, 18000, 9216, 6144, 3072,
        ])
        .unwrap();
    let mut scheduler = Scheduler::new(c, false, None).unwrap();
    assert!(scheduler.set_peak(peak).unwrap());
    {
        let state = scheduler.state.as_mut().unwrap();
        state.total_iters = 0;
        state.difficulty = candidate.difficulty;
        state.sub_slot_iters = candidate.sub_slot_iters;
        state.height = Some(12288);
    }
    scheduler.transaction_height = None;
    assert_eq!(
        scheduler.admit_unfinished(candidate.clone()).unwrap(),
        Admission::Deferred
    );
    let limit = scheduler.reward_limit();
    scheduler.deferred.clear();
    for key in 0..limit {
        scheduler
            .deferred
            .insert([key as u8; 32], candidate.clone());
    }
    assert_eq!(scheduler.deferred.len(), limit);
    assert_eq!(
        scheduler.admit_unfinished(candidate).unwrap(),
        Admission::DeferredQueueFull
    );
}
