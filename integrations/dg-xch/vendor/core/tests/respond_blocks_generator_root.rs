// Real-block coverage for the generator IDENTITY roots that `validate_body` checks, against the mainnet
// `RespondBlocks` fixture (heights 9138873–9138904, back-referenced generators).
//
//   transactions_generator_root(gen)               == transactions_info.generator_root
//   transactions_generator_refs_root(ref_list)     == transactions_info.generator_refs_root
//
// The execution test (`respond_blocks_generator_execution.rs`) proves execution→cost but never
// computes either root; these lock the identity roots against real mainnet blocks: the
// generator root is `std_hash(bytes(generator))` — never the decompressed tree hash.

use dg_xch_core::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::transactions_info::TransactionsInfo;
use dg_xch_core::clvm::program::SerializedProgram;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, TransactionBlockValidationInput,
    transactions_generator_refs_root, transactions_generator_root, validate_transaction_block,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use dg_xch_core::protocols::full_node::RespondBlocks;
use dg_xch_core::traits::SizedBytes;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

const RAW: &[u8] = include_bytes!("fixtures/respond_blocks_mainnet_9138873_9138904.bin");

fn decode() -> RespondBlocks {
    let mut cur = Cursor::new(RAW);
    RespondBlocks::from_bytes(&mut cur, ChiaProtocolVersion::default())
        .expect("real mainnet RespondBlocks must decode (back-reference-aware generator)")
}

/// Every transaction block's declared `generator_root` / `generator_refs_root` must be reproduced exactly.
#[test]
fn real_mainnet_generator_roots_match_declared() {
    let resp = decode();
    let mut checked = 0usize;
    let mut backref = 0usize;
    let mut with_refs = 0usize;

    for block in &resp.blocks {
        let Some(generator) = block.transactions_generator.as_ref() else {
            continue;
        };
        let Some(ti) = block.transactions_info.as_ref() else {
            continue;
        };
        let height = block.reward_chain_block.height;

        // gen_root: sha256 of the RAW serialized generator bytes — never the decompressed tree hash.
        let gen_root = transactions_generator_root(generator);
        assert_eq!(
            gen_root,
            ti.generator_root,
            "height {height}: generator_root mismatch\n computed = {}\n declared = {}",
            hex::encode(gen_root),
            hex::encode(ti.generator_root),
        );

        // refs_root: sha256 of the concatenated 4-byte-big-endian ref heights, or [1;32] if empty.
        let refs_root = transactions_generator_refs_root(&block.transactions_generator_ref_list)
            .expect("refs root");
        assert_eq!(
            refs_root,
            ti.generator_refs_root,
            "height {height}: generator_refs_root mismatch (refs={:?})\n computed = {}\n declared = {}",
            block.transactions_generator_ref_list,
            hex::encode(refs_root),
            hex::encode(ti.generator_refs_root),
        );

        // Coverage bookkeeping: a generator is back-referenced iff its canonical (no-backref) re-serialization
        // differs from the raw on-wire bytes. The root is taken over the RAW bytes, so this must still match.
        let decompressed = generator
            .to_program_backrefs()
            .unwrap_or_else(|e| panic!("height {height}: back-reference decode failed: {e:?}"));
        let canonical = decompressed
            .serialized()
            .expect("re-serialize decompressed");
        if canonical.as_ref() != generator.as_ref() {
            backref += 1;
        }
        if !block.transactions_generator_ref_list.is_empty() {
            with_refs += 1;
        }
        checked += 1;
    }

    assert!(
        checked >= 2,
        "fixture must contain >= 2 transaction-block generators; got {checked}",
    );
    assert!(
        backref >= 1,
        "fixture must contain >= 1 back-referenced generator (the generator-root hard-fork surface); got {backref}",
    );
    eprintln!(
        "generator-root coverage: checked={checked} back_referenced={backref} with_ref_list={with_refs}"
    );
}

/// End-to-end: run the REAL body validator (`validate_transaction_block`) on real mainnet transaction
/// blocks **with the real `FoliageTransactionBlock` attached** (`Some`). This drives, together, the whole
/// path the live node runs — execution, gen_root, refs_root, aggregate-signature verify, cost, fees,
/// `transactions_info_hash`, AND the foliage `additions_root`/`removals_root` merkle-set comparisons — and
/// must accept every block, returning the declared roots.
///
/// The `additions_root`/`removals_root` comparisons are the merkle-set surface: the additions root is a
/// `merkle_set` root over `(puzzle_hash, hash_coin_ids(coin_ids))` leaves and the removals root a merkle set
/// over spent-coin ids. Both are only reached once agg-sig, cost, and fees pass, so a green result here
/// proves the merkle-set node scheme is byte-exact on real blocks.
#[test]
fn real_mainnet_transaction_block_validates_end_to_end() {
    let resp = decode();
    let mut validated = 0usize;
    let mut with_foliage = 0usize;

    for block in &resp.blocks {
        let Some(generator) = block.transactions_generator.as_ref() else {
            continue;
        };
        // Self-contained only: a non-empty ref list needs prior generators the fixture does not carry.
        if !block.transactions_generator_ref_list.is_empty() {
            continue;
        }
        let Some(ti) = block.transactions_info.as_ref() else {
            continue;
        };
        let height = block.reward_chain_block.height;
        // The real foliage transaction block — this is what forces the additions/removals-root checks.
        let foliage = block.foliage_transaction_block.as_ref();
        if foliage.is_some() {
            with_foliage += 1;
        }

        let input = TransactionBlockValidationInput {
            prev_transaction_block_height: 0,
            generator_input: BlockGeneratorInput {
                transactions_generator: generator.clone(),
                generator_refs: Vec::new(),
                constants: MAINNET,
                height,
                flags: BlockGeneratorFlags::for_height(&MAINNET, height),
            },
            transactions_info: ti,
            foliage_transaction_block: foliage,
            condition_context: None,
        };

        let result = validate_transaction_block(&input).unwrap_or_else(|error| {
            panic!("height {height}: validate_transaction_block rejected a real mainnet block: {error:?}")
        });
        assert_eq!(
            result.generator_root, ti.generator_root,
            "height {height}: end-to-end generator_root must equal the declared root",
        );
        assert_eq!(
            result.generator_refs_root, ti.generator_refs_root,
            "height {height}: end-to-end generator_refs_root must equal the declared root",
        );
        assert_eq!(
            result.conditions.cost, ti.cost,
            "height {height}: end-to-end cost must equal transactions_info.cost",
        );
        // With real foliage attached, the declared additions/removals roots must equal ours.
        if let Some(foliage) = foliage {
            assert_eq!(
                result.additions_root, foliage.additions_root,
                "height {height}: end-to-end additions_root must equal the foliage additions_root",
            );
            assert_eq!(
                result.removals_root, foliage.removals_root,
                "height {height}: end-to-end removals_root must equal the foliage removals_root",
            );
        }
        validated += 1;
    }

    assert!(
        validated >= 2,
        "must fully validate >= 2 real transaction blocks end to end; got {validated}",
    );
    assert!(
        with_foliage >= 2,
        "must exercise the additions/removals-root checks on >= 2 blocks with real foliage; got {with_foliage}",
    );
    eprintln!(
        "end-to-end validate_transaction_block accepted {validated} real mainnet blocks ({with_foliage} with real foliage)"
    );
}

/// Targeted root coverage: on every real transaction block that carries a foliage transaction block, the
/// computed `additions_root` and `removals_root` must equal the block's declared foliage roots. This
/// isolates the additions merkle set (`hash_coin_ids`) and its sibling removals root from the
/// rest of the body path, so a regression in the merkle-set primitive fails here, not only end to end.
#[test]
fn real_mainnet_additions_and_removals_roots_match_foliage() {
    let resp = decode();
    let mut checked = 0usize;

    for block in &resp.blocks {
        let Some(generator) = block.transactions_generator.as_ref() else {
            continue;
        };
        if !block.transactions_generator_ref_list.is_empty() {
            continue;
        }
        let (Some(ti), Some(foliage)) = (
            block.transactions_info.as_ref(),
            block.foliage_transaction_block.as_ref(),
        ) else {
            continue;
        };
        let height = block.reward_chain_block.height;

        let input = TransactionBlockValidationInput {
            prev_transaction_block_height: 0,
            generator_input: BlockGeneratorInput {
                transactions_generator: generator.clone(),
                generator_refs: Vec::new(),
                constants: MAINNET,
                height,
                flags: BlockGeneratorFlags::for_height(&MAINNET, height),
            },
            transactions_info: ti,
            foliage_transaction_block: Some(foliage),
            condition_context: None,
        };

        let result = validate_transaction_block(&input)
            .unwrap_or_else(|error| panic!("height {height}: rejected: {error:?}"));
        assert_eq!(
            result.additions_root,
            foliage.additions_root,
            "height {height}: additions_root mismatch\n computed = {}\n declared = {}",
            hex::encode(result.additions_root),
            hex::encode(foliage.additions_root),
        );
        assert_eq!(
            result.removals_root,
            foliage.removals_root,
            "height {height}: removals_root mismatch\n computed = {}\n declared = {}",
            hex::encode(result.removals_root),
            hex::encode(foliage.removals_root),
        );
        checked += 1;
    }

    assert!(
        checked >= 2,
        "must confirm additions/removals roots on >= 2 real blocks; got {checked}",
    );
    eprintln!(
        "additions/removals-root coverage: matched declared foliage roots on {checked} real blocks"
    );
}

/// Tamper guard: corrupting the declared `additions_root` (resp. `removals_root`) in the foliage must make
/// `validate_transaction_block` reject with `BadAdditionRoot` (resp. `BadRemovalRoot`). This proves the root
/// checks are genuinely enforced and not loosened — a wrong root does not validate.
#[test]
fn tampered_foliage_roots_are_rejected() {
    let resp = decode();
    let block = resp
        .blocks
        .iter()
        .find(|b| {
            b.transactions_generator.is_some()
                && b.transactions_info.is_some()
                && b.foliage_transaction_block.is_some()
                && b.transactions_generator_ref_list.is_empty()
        })
        .expect("fixture has a self-contained transaction block with foliage");
    let generator = block.transactions_generator.as_ref().unwrap();
    let ti = block.transactions_info.as_ref().unwrap();
    let real_foliage = block.foliage_transaction_block.as_ref().unwrap();
    let height = block.reward_chain_block.height;

    // A free fn (not a closure) so the returned input's single lifetime unifies `ti` and `foliage`.
    fn make_input<'a>(
        generator: &SerializedProgram,
        ti: &'a TransactionsInfo,
        foliage: &'a FoliageTransactionBlock,
        height: u32,
    ) -> TransactionBlockValidationInput<'a> {
        TransactionBlockValidationInput {
            prev_transaction_block_height: 0,
            generator_input: BlockGeneratorInput {
                transactions_generator: generator.clone(),
                generator_refs: Vec::new(),
                constants: MAINNET,
                height,
                flags: BlockGeneratorFlags::for_height(&MAINNET, height),
            },
            transactions_info: ti,
            foliage_transaction_block: Some(foliage),
            condition_context: None,
        }
    }

    // Sanity: the untouched foliage validates.
    validate_transaction_block(&make_input(generator, ti, real_foliage, height))
        .expect("untouched real foliage must validate");

    // Corrupt the additions root -> BadAdditionRoot.
    let mut bad_additions = *real_foliage;
    let mut bytes = bad_additions.additions_root.bytes();
    bytes[0] ^= 0xff;
    bad_additions.additions_root = Bytes32::new(bytes);
    match validate_transaction_block(&make_input(generator, ti, &bad_additions, height)) {
        Err(ChiaError::BadAdditionRoot) => {}
        other => panic!("tampered additions_root must fail BadAdditionRoot; got {other:?}"),
    }

    // Corrupt the removals root -> BadRemovalRoot (additions root left intact so we reach the check).
    let mut bad_removals = *real_foliage;
    let mut bytes = bad_removals.removals_root.bytes();
    bytes[0] ^= 0xff;
    bad_removals.removals_root = Bytes32::new(bytes);
    match validate_transaction_block(&make_input(generator, ti, &bad_removals, height)) {
        Err(ChiaError::BadRemovalRoot) => {}
        other => panic!("tampered removals_root must fail BadRemovalRoot; got {other:?}"),
    }
}

/// Guard: a genuinely wrong generator must STILL be rejected. Tamper one byte of a real generator; its
/// sha256 root necessarily changes, so `validate_transaction_block` must reject with
/// `InvalidTransactionsGeneratorHash`.
#[test]
fn tampered_generator_is_still_rejected() {
    let resp = decode();
    let block = resp
        .blocks
        .iter()
        .find(|b| {
            b.transactions_generator.is_some()
                && b.transactions_info.is_some()
                && b.transactions_generator_ref_list.is_empty()
        })
        .expect("fixture has a self-contained transaction block");
    let generator = block.transactions_generator.as_ref().unwrap();
    let ti = block.transactions_info.as_ref().unwrap();
    let height = block.reward_chain_block.height;

    // The real generator hashes to the declared root — sanity anchor for the tamper.
    assert_eq!(transactions_generator_root(generator), ti.generator_root);

    // Flip the final byte; a different byte string yields a different sha256, so the root no longer matches.
    let mut bytes = generator.as_ref().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let tampered = SerializedProgram::from(bytes);
    assert_ne!(
        transactions_generator_root(&tampered),
        ti.generator_root,
        "a tampered generator must not share the declared root",
    );

    let input = TransactionBlockValidationInput {
        prev_transaction_block_height: 0,
        generator_input: BlockGeneratorInput {
            transactions_generator: tampered,
            generator_refs: Vec::new(),
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        },
        transactions_info: ti,
        foliage_transaction_block: block.foliage_transaction_block.as_ref(),
        condition_context: None,
    };

    // The tampered generator either fails execution (a corrupted CLVM stream) or, if it still runs, fails the
    // gen_root check. Either way it must be rejected — never accepted. When it runs, the rejection must be
    // exactly InvalidTransactionsGeneratorHash.
    match validate_transaction_block(&input) {
        Ok(result) => panic!(
            "tampered generator must be rejected; instead validated with root {}",
            hex::encode(result.generator_root)
        ),
        Err(ChiaError::InvalidTransactionsGeneratorHash) => {}
        Err(other) => {
            // Acceptable only if execution itself rejected the corrupted stream before the root check;
            // assert it is a rejection, not a silent pass (it is, since we are in the Err arm).
            eprintln!("tampered generator rejected earlier in the body path: {other:?}");
        }
    }
}
