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
    scheduler.deferred.insert(
        candidate.reward_chain_block.hash().unwrap().const_bytes(),
        candidate.clone(),
    );
    let mut invalid = candidate;
    invalid.reward_chain_block.proof_of_space =
        serde_json::from_value(data["rows"][0]["reward_block"]["proof_of_space"].clone()).unwrap();
    invalid.reward_chain_block.proof_of_space.proof =
        serde_json::from_value(serde_json::json!("0x00")).unwrap();
    scheduler.deferred.insert([0; 32], invalid);
    assert!(scheduler.set_peak(peak.clone()).unwrap());
    assert!(scheduler.deferred.is_empty());
    assert_eq!(scheduler.candidates.len(), 1);
    assert!(scheduler.next_plan().unwrap().is_some());
    // A noncontiguous, non-transaction peak must lose transaction context rather
    // than inventing a previous transaction height from the candidate height.
    let mut later = peak;
    later.reward_chain_block.height += 2;
    later.reward_chain_block.is_transaction_block = false;
    assert!(scheduler.set_peak(later).unwrap());
    assert_eq!(scheduler.transaction_height, None);
    assert!(scheduler.next_plan().unwrap().is_some());
}
