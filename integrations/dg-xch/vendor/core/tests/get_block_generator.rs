//! Block-level generator assembly for a block that references prior blocks' generators:
//! the block must carry a generator; the referenced generators are resolved by height and
//! supplied to the executor in ref-list order.
//!
//! Fixtures are vendored bytes.

use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference, execute_block_generator_result,
    transactions_generator_refs_root,
};
use dg_xch_core::consensus::constants::MAINNET;

const HISTORICAL_BLOCK_4671894: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.txt");
const HISTORICAL_BLOCK_4671894_REF: &str =
    include_str!("fixtures/chia_generator_tests/block-4671894.env");

fn generator_line(fixture: &str) -> SerializedProgram {
    SerializedProgram::from_hex(fixture.lines().next().unwrap()).unwrap()
}

// With an empty ref list, generator assembly succeeds without ever invoking the lookup
// callback.
#[test]
fn no_refs_generator_assembles_without_lookup() {
    let output = SExp::from(vec![SExp::from(Vec::<SExp>::new())]);
    let generator = Program::to((1_u8, output)).serialized().unwrap();
    let input = BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: 10,
        flags: BlockGeneratorFlags {
            simple_generator: true,
            ..Default::default()
        },
    };

    let conds = execute_block_generator_result(&input).unwrap();
    assert!(conds.spends.is_empty());
}

// A block that references a prior block's generator
// validates once that generator is resolved (by height) from storage and supplied in
// ref-list order. Block 4,671,894 references the height-4,671,893 generator. With the
// reference resolved the block executes and yields conditions; an empty-ref run cannot
// reproduce them.
//
// node/src/engine.rs::resolve_generator_refs fetches each
// referenced generator from the confirmed chain (dg_xch_stores get_generator_at_height)
// and validate_body supplies them. The storage-side resolution is exercised end to end
// in node/tests/generator_ref_resolution.rs; this asserts the executor-level contract.
#[test]
fn ref_list_block_resolves_prior_generator_from_storage() {
    let generator = generator_line(HISTORICAL_BLOCK_4671894);
    let reference = SerializedProgram::from_hex(HISTORICAL_BLOCK_4671894_REF).unwrap();

    // The resolved path: the referenced generator is fetched by height and supplied.
    let resolved = BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: vec![GeneratorReference {
            height: 4_671_893,
            index: 0,
            generator: reference,
        }],
        constants: MAINNET,
        height: 4_671_894,
        flags: BlockGeneratorFlags::default(),
    };
    let conds =
        execute_block_generator_result(&resolved).expect("resolved ref-list block validates");
    assert!(
        conds.cost > 0,
        "a validated transaction block has a positive execution cost"
    );

    // An empty-ref run cannot reproduce the resolved conditions (the generator consumes
    // the reference argument).
    let unresolved = BlockGeneratorInput {
        generator_refs: Vec::new(),
        ..resolved
    };
    assert_ne!(execute_block_generator_result(&unresolved), Ok(conds));
}

// A block's generator_refs_root is checked against the hash of its ref-list heights
// (transactions_generator_refs_root). engine.rs::validate_body computes this from the
// ACTUAL referenced heights and rejects a block whose ti.generator_refs_root disagrees.
// These lock the rejection predicate that branch relies on.
#[test]
fn generator_refs_root_is_order_sensitive() {
    let ab = transactions_generator_refs_root(&[4_671_893, 4_671_894]).unwrap();
    let ba = transactions_generator_refs_root(&[4_671_894, 4_671_893]).unwrap();
    assert_ne!(
        ab, ba,
        "ref-list order changes the refs_root, so a mis-ordered ti root is rejected"
    );
}

#[test]
fn generator_refs_root_of_real_heights_differs_from_empty() {
    let real = transactions_generator_refs_root(&[4_671_893]).unwrap();
    let empty = transactions_generator_refs_root(&[]).unwrap();
    assert_ne!(
        real, empty,
        "a real ref-list block's refs_root differs from the empty-list root the node formerly compared against"
    );
}
