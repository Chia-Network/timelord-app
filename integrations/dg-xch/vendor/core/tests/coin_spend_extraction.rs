use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, additions_for_conditions, coin_spend_from_generator,
    execute_block_generator_result, hints_for_conditions,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_core::traits::SizedBytes;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::collections::HashSet;
use std::io::Cursor;

const RAW: &[u8] = include_bytes!("fixtures/respond_blocks_mainnet_9138873_9138904.bin");

fn decode() -> RespondBlocks {
    let mut cur = Cursor::new(RAW);
    RespondBlocks::from_bytes(&mut cur, ChiaProtocolVersion::default())
        .expect("real mainnet RespondBlocks must decode")
}

fn generator_input(
    block: &dg_xch_core::blockchain::full_block::FullBlock,
) -> Option<BlockGeneratorInput> {
    block.transactions_generator.as_ref()?;
    if !block.transactions_generator_ref_list.is_empty() {
        return None; // self-contained blocks only (the fixture carries no prior generators)
    }
    let height = block.reward_chain_block.height;
    Some(BlockGeneratorInput {
        transactions_generator: block.transactions_generator.clone().expect("checked above"),
        generator_refs: Vec::new(),
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    })
}

// For every real spend in the fixture, the extracted CoinSpend must (a) name the same coin and
// (b) re-run to the SAME additions the block generator derived for that coin — proof the reveal +
// solution are the genuine ones, not merely a coin with a matching id.
#[test]
fn extracted_coin_spend_round_trips_against_generator() {
    let resp = decode();
    let mut spends_checked = 0_usize;
    let mut blocks_checked = 0_usize;

    for block in &resp.blocks {
        let Some(input) = generator_input(block) else {
            continue;
        };
        let conds =
            execute_block_generator_result(&input).expect("fixture transaction block must execute");
        blocks_checked += 1;

        for spend in &conds.spends {
            // Additions this spend produced, per the validated conditions (Bytes32 has no Ord, so
            // compare as sets).
            let expected: HashSet<Bytes32> = single_spend_additions(&conds, spend.coin_id)
                .into_iter()
                .collect();

            let extracted = coin_spend_from_generator(&input, &spend.coin_id)
                .expect("simple-generator extraction must not error")
                .expect("a spent coin must resolve to a CoinSpend");
            assert_eq!(
                extracted.coin.name(),
                spend.coin_id,
                "extracted CoinSpend must name the queried coin",
            );
            let got: HashSet<Bytes32> = extracted
                .additions()
                .expect("re-running the reveal over the solution must succeed")
                .iter()
                .map(dg_xch_core::blockchain::coin::Coin::name)
                .collect();
            assert_eq!(
                got, expected,
                "reveal+solution must reproduce the coin's on-chain additions",
            );
            spends_checked += 1;
        }
    }

    assert!(
        blocks_checked >= 2 && spends_checked >= 2,
        "fixture must exercise multiple real spends across multiple blocks; \
         got {spends_checked} spends over {blocks_checked} blocks",
    );
}

// A coin id absent from the block yields Ok(None) — never a spurious match or panic.
#[test]
fn absent_coin_yields_none() {
    let resp = decode();
    let input = resp
        .blocks
        .iter()
        .find_map(generator_input)
        .expect("fixture must carry a self-contained transaction block");
    let absent = Bytes32::new([0xAB; 32]);
    assert!(
        coin_spend_from_generator(&input, &absent)
            .expect("extraction must not error")
            .is_none(),
        "a coin the block never spends must resolve to None",
    );
}

// The hint index feed must agree with the validated conditions: every emitted (hint, coin_id) pair
// names a real created coin whose CreateCoin memo is exactly those 32 bytes, and no shorter/longer
// hint is emitted.
#[test]
fn hints_match_created_coins() {
    let resp = decode();
    let mut total_hints = 0_usize;

    for block in &resp.blocks {
        let Some(input) = generator_input(block) else {
            continue;
        };
        let conds =
            execute_block_generator_result(&input).expect("fixture transaction block must execute");

        for (hint, coin_id) in hints_for_conditions(&conds) {
            total_hints += 1;
            // Locate the create-coin this hint points at and confirm the memo matches.
            let found = conds.spends.iter().any(|spend| {
                spend.create_coin.iter().any(|c| {
                    let coin = dg_xch_core::blockchain::coin::Coin {
                        parent_coin_info: spend.coin_id,
                        puzzle_hash: c.puzzle_hash,
                        amount: c.amount,
                    };
                    coin.name() == coin_id
                        && c.hint
                            .as_ref()
                            .map(dg_xch_core::blockchain::unsized_bytes::UnsizedBytes::as_slice)
                            == Some(hint.as_ref())
                })
            });
            assert!(
                found,
                "emitted hint must name a real create-coin with that exact 32-byte memo"
            );
        }
    }

    // Data-dependent: the fixture is real mainnet traffic. Report the count so a zero is visible
    // rather than silently passing on an empty extractor.
    println!("hints_for_conditions emitted {total_hints} 32-byte hint pairs across the fixture");
}

// Additions produced by a single spend (identified by its coin id), reconstructed from conds the
// same way `additions_for_conditions` builds the block-wide set.
fn single_spend_additions(
    conds: &dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions,
    coin_id: Bytes32,
) -> Vec<Bytes32> {
    let Some(spend) = conds.spends.iter().find(|s| s.coin_id == coin_id) else {
        return Vec::new();
    };
    let mut single = conds.clone();
    single.spends = vec![spend.clone()];
    additions_for_conditions(&single, &[])
        .iter()
        .map(dg_xch_core::blockchain::coin::Coin::name)
        .collect()
}
