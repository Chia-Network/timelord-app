// Modified by Chia Network for timelord-app; see integrations/dg-xch/VENDORING.md
use dg_xch_core::{
    blockchain::{
        challenge_block_info::ChallengeBlockInfo, proof_of_space::ProofOfSpace,
        reward_chain_block::RewardChainBlock, sized_bytes::Bytes32,
    },
    consensus::{
        constants::{ConsensusConstants, MAINNET},
        filter_schedule::FilterSchedule,
        pot_iterations::{
            TxContext, calculate_timelord_iterations, timelord_iterations_with_context,
        },
    },
    protocols::{NodeType, timelord::NewUnfinishedBlockTimelord},
};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde_json::Value;
use std::io::Cursor;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/chia_hf2.json")).unwrap()
}
fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}
fn hash(value: &Value) -> Bytes32 {
    serde_json::from_value(value.clone()).unwrap()
}
#[test]
fn chia_wire_hash_quality_and_iteration_vectors() {
    let data = fixture();
    let version = ChiaProtocolVersion::default();
    let mut c = MAINNET;
    c.min_plot_size = 18;
    c.hard_fork_height = 0;
    c.epoch_blocks = 768;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_plot_strength = 9;
    c.difficulty_constant_factor = 10052721566054;
    c.plot_filter_v2_relative_height =
        dg_xch_core::consensus::filter_schedule::FilterSchedule::try_from_slice(&[
            33000, 30000, 27000, 24000, 21000, 18000, 9216, 6144, 3072,
        ])
        .unwrap();
    let first = &data["rows"][0];
    let mut pos: ProofOfSpace =
        serde_json::from_value(first["reward_block"]["proof_of_space"].clone()).unwrap();
    for vector in data["iteration_vectors"].as_array().unwrap() {
        pos.strength = vector["strength"].as_u64().unwrap() as u8;
        assert_eq!(
            calculate_timelord_iterations(
                &c,
                &pos,
                hash(&first["quality"]),
                536,
                hash(&first["signage_hash"]),
                vector["height"].as_u64().unwrap() as u32
            )
            .unwrap(),
            vector["required_iters"].as_u64().unwrap(),
            "{vector}"
        );
    }
    for row in data["rows"].as_array().unwrap() {
        let block: RewardChainBlock = serde_json::from_value(row["reward_block"].clone()).unwrap();
        let raw = bytes(row["reward_bytes"].as_str().unwrap());
        assert_eq!(block.to_bytes(version).unwrap(), raw);
        let mut cursor = Cursor::new(raw.as_slice());
        let decoded = RewardChainBlock::from_bytes(&mut cursor, version).unwrap();
        assert_eq!(cursor.position(), raw.len() as u64);
        assert_eq!(block, decoded);
        let tail = 1
            + usize::from(block.header_mmr_root.is_some()) * 32
            + block
                .infused_challenge_chain_ip_vdf
                .as_ref()
                .map(|v| v.to_bytes(version).unwrap().len())
                .unwrap_or(0);
        let flag_offset = raw.len() - tail - 1;
        let mut malformed = raw.clone();
        malformed[flag_offset] = 4;
        assert!(
            RewardChainBlock::from_bytes(&mut Cursor::new(malformed.as_slice()), version).is_err()
        );
        assert!(
            RewardChainBlock::from_bytes(&mut Cursor::new(&raw[..raw.len() - 1]), version).is_err()
        );
        assert_eq!(block.hash().unwrap(), hash(&row["reward_hash"]));
        assert_eq!(
            block.get_unfinished().hash().unwrap(),
            hash(&row["unfinished_reward_hash"])
        );
        let challenge = ChallengeBlockInfo {
            proof_of_space: block.proof_of_space.clone(),
            challenge_chain_sp_vdf: block.challenge_chain_sp_vdf,
            challenge_chain_sp_signature: block.challenge_chain_sp_signature,
            challenge_chain_ip_vdf: block.challenge_chain_ip_vdf,
        };
        assert_eq!(
            challenge.hash().unwrap(),
            hash(&row["challenge_block_hash"])
        );
        let mut no_root = block.clone();
        no_root.header_mmr_root = None;
        assert_eq!(
            no_root.to_bytes(version).unwrap(),
            bytes(row["no_root_bytes"].as_str().unwrap())
        );
        assert_eq!(no_root.hash().unwrap(), hash(&row["no_root_hash"]));
        let raw = bytes(row["unfinished_bytes"].as_str().unwrap());
        let mut cursor = Cursor::new(raw.as_slice());
        let unfinished = NewUnfinishedBlockTimelord::from_bytes(&mut cursor, version).unwrap();
        assert_eq!(cursor.position(), raw.len() as u64);
        assert_eq!(unfinished.header_mmr_root, block.header_mmr_root);
        let signage = hash(&row["signage_hash"]);
        let quality = if block.proof_of_space.version == 1 {
            block
                .proof_of_space
                .chia_timelord_quality(&c, block.pos_ss_cc_challenge_hash, signage)
        } else {
            dg_xch_pos::verify_and_get_quality_string_without_activation(
                &block.proof_of_space,
                &c,
                block.pos_ss_cc_challenge_hash,
                signage,
                block.height,
            )
        }
        .unwrap();
        assert_eq!(quality, hash(&row["quality"]));
        assert_eq!(
            calculate_timelord_iterations(
                &c,
                &block.proof_of_space,
                quality,
                536,
                signage,
                block.height - 1
            )
            .unwrap(),
            row["required_iters"].as_u64().unwrap()
        );
        if block.proof_of_space.version == 1 {
            let mut corrupt = serde_json::to_value(&block.proof_of_space).unwrap();
            corrupt["proof"] = serde_json::json!("0x00");
            let corrupt: ProofOfSpace = serde_json::from_value(corrupt).unwrap();
            assert!(
                corrupt
                    .chia_timelord_quality(&c, block.pos_ss_cc_challenge_hash, signage)
                    .is_none()
            );
            assert!(corrupt.consensus_bytes(version).is_err());
            assert!(
                block
                    .proof_of_space
                    .chia_timelord_quality(&c, Bytes32::const_new([9; 32]), signage)
                    .is_none()
            );
        }
    }
}
#[test]
fn capability_is_scoped_to_timelord_role() {
    let enabled = dg_xch_timelord::service::timelord_handshake("mainnet");
    assert_eq!(
        enabled
            .capabilities
            .iter()
            .filter(|capability| capability.0 == 6)
            .count(),
        1
    );
    assert!(enabled.capabilities.contains(&(6, "1".to_owned())));
    let other = dg_xch_servers::transport::handshake("mainnet", NodeType::FullNode, 0);
    assert!(!other.capabilities.iter().any(|c| c.0 == 6));
}

#[test]
fn current_chia_peak_envelope() {
    let data = fixture();
    let raw = bytes(data["captured_peak"]["bytes"].as_str().unwrap());
    let mut cursor = Cursor::new(raw.as_slice());
    let result = dg_xch_core::protocols::timelord::NewPeakTimelord::from_bytes(
        &mut cursor,
        ChiaProtocolVersion::default(),
    );
    assert!(result.is_ok(), "offset {}: {:?}", cursor.position(), result);
    assert_eq!(cursor.position(), raw.len() as u64);
    let peak = result.unwrap();
    let mut constants = MAINNET;
    constants.min_plot_size = 18;
    constants.hard_fork2_height = 9216;
    let mut scheduler = dg_xch_timelord::scheduler::Scheduler::new(constants, false, None).unwrap();
    assert!(scheduler.set_peak(peak).unwrap());
}

#[test]
fn captured_peer_sequence_schedules_real_unfinished_blocks() {
    let data = fixture();
    let mut c = MAINNET;
    c.min_plot_size = 18;
    c.hard_fork_height = 0;
    c.epoch_blocks = 768;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_plot_strength = 9;
    c.difficulty_constant_factor = 10052721566054;
    c.plot_filter_v2_relative_height =
        dg_xch_core::consensus::filter_schedule::FilterSchedule::try_from_slice(&[
            33000, 30000, 27000, 24000, 21000, 18000, 9216, 6144, 3072,
        ])
        .unwrap();
    let mut scheduler = dg_xch_timelord::scheduler::Scheduler::new(c, false, None).unwrap();
    let mut scheduled = 0;
    for message in data["peer_sequence"].as_array().unwrap() {
        let raw = bytes(message["bytes"].as_str().unwrap());
        let mut cursor = Cursor::new(raw.as_slice());
        if message["kind"] == 13 {
            let peak = dg_xch_core::protocols::timelord::NewPeakTimelord::from_bytes(
                &mut cursor,
                ChiaProtocolVersion::default(),
            )
            .unwrap();
            assert!(scheduler.set_peak(peak).unwrap());
        } else {
            let unfinished =
                NewUnfinishedBlockTimelord::from_bytes(&mut cursor, ChiaProtocolVersion::default())
                    .unwrap();
            scheduled += usize::from(scheduler.add_unfinished(unfinished).unwrap());
        }
        assert_eq!(cursor.position(), raw.len() as u64);
    }
    assert!(scheduled > 0);
}

fn context_fixture() -> (ConsensusConstants, ProofOfSpace, Bytes32, Bytes32) {
    let data = fixture();
    let first = &data["rows"][0];
    let mut pos: ProofOfSpace =
        serde_json::from_value(first["reward_block"]["proof_of_space"].clone()).unwrap();
    let mut c = MAINNET;
    c.hard_fork2_height = 9216;
    c.number_zero_bits_plot_filter_v2 = 7;
    c.max_effective_plot_filter_bits = 13;
    c.difficulty_constant_factor = 10052721566054;
    c.plot_filter_v2_relative_height = FilterSchedule::try_from_slice(&[
        33000, 30000, 27000, 24000, 21000, 18000, 9216, 6144, 3072,
    ])
    .unwrap();
    pos.strength = 9;
    let q = hash(&first["quality"]);
    let sp = hash(&first["signage_hash"]);
    (c, pos, q, sp)
}

fn context(known: Option<u32>, lower: u32, upper: u32) -> TxContext {
    TxContext {
        known,
        lower,
        upper,
    }
}

#[test]
fn unknown_transaction_context_requires_an_invariant_target() {
    let (c, pos, q, sp) = context_fixture();
    let before =
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12287), 0, 12288))
            .unwrap()
            .unwrap();
    let after =
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12288), 0, 12288))
            .unwrap()
            .unwrap();
    assert_ne!(before, after);
    assert_eq!(
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(None, 0, 12288)).unwrap(),
        None
    );
    let mut low_strength = pos;
    low_strength.strength = 2;
    assert!(
        timelord_iterations_with_context(&c, &low_strength, q, 536, sp, context(None, 0, 50_000))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_boundary_below_the_known_lower_bound_does_not_defer() {
    let (c, pos, q, sp) = context_fixture();
    let at_lower =
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12288), 0, 12288))
            .unwrap()
            .unwrap();
    // The 12288 boundary is not strictly above lower, so only the target at lower matters.
    assert_eq!(
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(None, 12288, 13_000))
            .unwrap(),
        Some(at_lower)
    );
}

#[test]
fn a_boundary_inside_the_unknown_range_still_defers() {
    let (c, pos, q, sp) = context_fixture();
    let at_lower =
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12287), 0, 12287))
            .unwrap()
            .unwrap();
    // The 12288 boundary lies in (12287, 12288], and changes the target.
    assert_eq!(
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(None, 12287, 12288))
            .unwrap(),
        None
    );
    assert_ne!(
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12288), 0, 12288))
            .unwrap()
            .unwrap(),
        at_lower
    );
}

#[test]
fn a_boundary_at_the_candidate_height_does_not_defer() {
    let (c, pos, q, sp) = context_fixture();
    let before =
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(Some(12287), 0, 12287))
            .unwrap()
            .unwrap();
    // Candidate height is peak + 1 = 12288, so the scheduler's upper bound is the peak (12287).
    assert_eq!(
        timelord_iterations_with_context(&c, &pos, q, 536, sp, context(None, 12287, 12287))
            .unwrap(),
        Some(before)
    );
}
