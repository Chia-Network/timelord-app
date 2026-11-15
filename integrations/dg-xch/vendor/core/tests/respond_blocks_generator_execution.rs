// Execution coverage for the real-wire block generators in the mainnet `RespondBlocks` fixture.
//
// `respond_blocks_real_wire.rs` proves only that the back-reference-aware decoder consumes the exact
// on-wire byte length and that the raw bytes round-trip. It never runs the generator: the ChiaSerialize
// decode keeps the RAW slice and discards the parsed tree, so a back-reference that resolves to the wrong
// (but same-length) subtree would still pass that round-trip yet fail consensus.
//
// Here we DECOMPRESS the generator (`to_program_backrefs`), RUN it, and drive the full body path
// (`execute_block_generator_result`), asserting the produced cost equals the block's own
// `transactions_info.cost`. Stages are split so a failure names the stage (decode vs run vs conditions vs
// cost) instead of collapsing to `InvalidBlockSolution`.

use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::utils::{COST_CONDITIONS, ENABLE_KECCAK_OPS_OUTSIDE_FORK};
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, execute_block_generator,
    execute_block_generator_result,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

const RAW: &[u8] = include_bytes!("fixtures/respond_blocks_mainnet_9138873_9138904.bin");

fn decode() -> RespondBlocks {
    let mut cur = Cursor::new(RAW);
    RespondBlocks::from_bytes(&mut cur, ChiaProtocolVersion::default())
        .expect("real mainnet RespondBlocks must decode (back-reference-aware generator)")
}

#[test]
fn real_mainnet_generator_executes_to_block_cost() {
    let resp = decode();
    let mut checked = 0_usize;

    for block in &resp.blocks {
        // Only self-contained transaction blocks: a non-empty ref list needs prior generators the
        // fixture does not carry, so it is out of scope for this reproduction.
        let Some(generator) = block.transactions_generator.as_ref() else {
            continue;
        };
        if !block.transactions_generator_ref_list.is_empty() {
            continue;
        }
        let ti = block
            .transactions_info
            .as_ref()
            .expect("transaction block must carry transactions_info");
        let height = block.reward_chain_block.height;

        // Stage 1 — decompress the CLVM back-references into a runnable program.
        let program = generator.to_program_backrefs().unwrap_or_else(|error| {
            panic!("height {height}: back-reference decode failed: {error:?}")
        });

        // Stage 2 — run the post-hard-fork simple generator with NIL args.
        let flags = COST_CONDITIONS | ENABLE_KECCAK_OPS_OUTSIDE_FORK;
        let run = program.run(MAINNET.max_block_cost_clvm, flags, &Program::default());
        assert!(
            run.is_ok(),
            "height {height}: generator run failed: {:?}",
            run.err()
        );

        // Stage 3 — full body path must reproduce the block's own transactions_info.cost.
        let input = BlockGeneratorInput {
            transactions_generator: generator.clone(),
            generator_refs: Vec::new(),
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        };
        let conds = execute_block_generator_result(&input).unwrap_or_else(|error| {
            panic!("height {height}: execute_block_generator_result rejected the block: {error:?}")
        });
        assert_eq!(
            conds.cost, ti.cost,
            "height {height}: generator cost must equal transactions_info.cost",
        );
        checked += 1;
    }

    // The fixture carries several self-contained transaction blocks (heights 9138874, 9138880, ...).
    // Require more than one so the assertion covers multiple real blocks, not a single fixture.
    assert!(
        checked >= 2,
        "fixture must contain at least two self-contained transaction-block generators to execute; got {checked}",
    );
}

// A genuinely bad generator must still be rejected as InvalidBlockSolution — the error-surfacing
// refinement (map_condition_error) must not turn a malformed solution into a false accept or a
// different error class. We use a structurally valid simple generator (`(q . ())`, which passes the
// 0xff01 / operator-`1` simple-generator checks and runs cleanly) whose OUTPUT is nil rather than a
// spend list, so condition extraction fails and must map to InvalidBlockSolution.
#[test]
fn corrupted_generator_is_still_rejected() {
    // `ff0180` == `(1 . ())`: quote returning nil. It runs, but its output is an atom, not the
    // `((parent puzzle amount solution) ...)` spend list the body path requires.
    let bad = SerializedProgram::from_hex("ff0180").expect("hand-built generator hex is valid");

    let input = BlockGeneratorInput {
        transactions_generator: bad,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: 9_138_874,
        flags: BlockGeneratorFlags::for_height(&MAINNET, 9_138_874),
    };

    match execute_block_generator_result(&input) {
        Ok(conds) => {
            panic!(
                "bad generator must not validate; produced cost {}",
                conds.cost
            )
        }
        Err(error) => assert_eq!(
            error,
            ChiaError::InvalidBlockSolution,
            "a generator that does not yield a spend list must be rejected as InvalidBlockSolution",
        ),
    }

    // The NPCResult wrapper must surface the matching Err code (2), never a silent accept.
    let npc = execute_block_generator(&input);
    assert!(
        npc.conds.is_none(),
        "bad generator must yield no conditions"
    );
    assert_eq!(
        npc.error,
        Some(2),
        "InvalidBlockSolution maps to error code 2"
    );
}
