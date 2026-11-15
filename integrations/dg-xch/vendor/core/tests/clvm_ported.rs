//! CLVM VM + generator tests ported from chia-blockchain's test corpus.
//!
//! Sources (chia-blockchain @ `chia/_tests/`):
//!   * `clvm/test_chialisp_deserialization.py` — the CHIALISP_DESERIALISATION
//!     CLVM program deserializing on-wire blobs.
//!   * `generator/test_compression.py::TestDecompression` — deserialization
//!     directly and as a generator argument.
//!   * `generator/test_generator_types.py::test_make_generator_args`.
//!
//! Chia asserts these by running the deserializer *inside* CLVM; we do the same
//! against dg_xch's hand-rolled VM (`Program::run`) and cross-check against the
//! native `sexp_from_bytes` parser. Static fixtures are vendored under
//! `core/tests/fixtures/clvm/`.

use dg_xch_core::clvm::parser::{sexp_from_bytes, sexp_from_bytes_backrefs, sexp_to_bytes};
use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::clvm::utils::INFINITE_COST;
use dg_xch_core::consensus::generator_puzzles::CHIALISP_DESERIALISATION_HEX;
use std::io::Cursor;

const TEST_GEN_DESERIALIZE_HEX: &str =
    include_str!("fixtures/clvm/test_generator_deserialize.clsp.hex");
const TEST_MULTIPLE_HEX: &str =
    include_str!("fixtures/clvm/test_multiple_generator_input_arguments.clsp.hex");
const CLVM_GENERATOR_BIN: &[u8] = include_bytes!("fixtures/clvm/clvm_generator.bin");

// gen1 from test_generator_types.py
const GEN1_HEX: &str = "ff01ffffffa00000000000000000000000000000000000000000000000000000000000000000ff830186a080ffffff02ffff01ff02ffff01ff02ffff03ff0bffff01ff02ffff03ffff09ff05ffff1dff0bffff1effff0bff0bffff02ff06ffff04ff02ffff04ff17ff8080808080808080ffff01ff02ff17ff2f80ffff01ff088080ff0180ffff01ff04ffff04ff04ffff04ff05ffff04ffff02ff06ffff04ff02ffff04ff17ff80808080ff80808080ffff02ff17ff2f808080ff0180ffff04ffff01ff32ff02ffff03ffff07ff0580ffff01ff0bffff0102ffff02ff06ffff04ff02ffff04ff09ff80808080ffff02ff06ffff04ff02ffff04ff0dff8080808080ffff01ff0bffff0101ff058080ff0180ff018080ffff04ffff01b081963921826355dcb6c355ccf9c2637c18adf7d38ee44d803ea9ca41587e48c913d8d46896eb830aeadfc13144a8eac3ff018080ffff80ffff01ffff33ffa06b7a83babea1eec790c947db4464ab657dbe9b887fe9acc247062847b8c2a8a9ff830186a08080ff8080808080";

fn deserialize_mod() -> SerializedProgram {
    SerializedProgram::from_hex(CHIALISP_DESERIALISATION_HEX).unwrap()
}

// Run DESERIALIZE_MOD on a single wire blob, exactly as chia's
// `DESERIALIZE_MOD.run_with_cost(INFINITE_COST, [b])`.
fn run_deserialize(blob: &[u8]) -> Result<Program<'static>, dg_xch_core::errors::ClvmError> {
    let serial = deserialize_mod();
    let program = serial.to_program()?;
    let args = Program::new(SExp::from(vec![SExp::from(blob.to_vec())]));
    program.run(INFINITE_COST, 0, &args).map(|(_c, out)| out)
}

// Ports serialized_atom_overflow() from test_chialisp_deserialization.py.
fn serialized_atom_overflow(size: u64) -> Vec<u8> {
    let mut size_blob: Vec<u8> = if size == 0 {
        vec![0x80]
    } else if size < 0x40 {
        vec![0x80 | size as u8]
    } else if size < 0x2000 {
        vec![0xC0 | (size >> 8) as u8, size as u8]
    } else if size < 0x0010_0000 {
        vec![0xE0 | (size >> 16) as u8, (size >> 8) as u8, size as u8]
    } else if size < 0x0800_0000 {
        vec![
            0xF0 | (size >> 24) as u8,
            (size >> 16) as u8,
            (size >> 8) as u8,
            size as u8,
        ]
    } else if size < 0x0004_0000_0000 {
        vec![
            0xF8 | (size >> 32) as u8,
            (size >> 24) as u8,
            (size >> 16) as u8,
            (size >> 8) as u8,
            size as u8,
        ]
    } else {
        vec![
            0xFC | ((size >> 40) & 0xFF) as u8,
            (size >> 32) as u8,
            (size >> 24) as u8,
            (size >> 16) as u8,
            (size >> 8) as u8,
            size as u8,
        ]
    };
    size_blob.extend(std::iter::repeat_n(0x01_u8, 1000));
    size_blob
}

// test_chialisp_deserialization.py::test_deserialization_simple_list
#[test]
fn vm_deserialize_simple_list() {
    let b = hex::decode("ff8568656c6c6fff86667269656e6480").unwrap();
    let out = run_deserialize(&b).unwrap();
    let expected = Program::new(sexp_from_bytes(&mut Cursor::new(b.as_slice())).unwrap());
    assert_eq!(out, expected);
}

// test_chialisp_deserialization.py::test_deserialization_password_coin
#[test]
fn vm_deserialize_password_coin() {
    let b = hex::decode(
        "ff04ffff0affff0bff0280ffff01ffa02cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b98248080ffff05ffff01ff3380ffff05ff05ffff05ffff01ff6480ffff01ff8080808080ffff01ff8e77726f6e672070617373776f72648080",
    )
    .unwrap();
    let out = run_deserialize(&b).unwrap();
    let expected = Program::new(sexp_from_bytes(&mut Cursor::new(b.as_slice())).unwrap());
    assert_eq!(out, expected);
}

// test_chialisp_deserialization.py::test_deserialization_large_numbers
#[test]
fn vm_deserialize_large_numbers() {
    let b = hex::decode(
        "ff9c00f316271c7fc3908a8bef464e3945ef7a253609ffffffffffffffffffb00fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa1ff22ea0179500526edb610f148ec0c614155678491902d6000000000000000000180",
    )
    .unwrap();
    let out = run_deserialize(&b).unwrap();
    let expected = Program::new(sexp_from_bytes(&mut Cursor::new(b.as_slice())).unwrap());
    assert_eq!(out, expected);
}

// test_chialisp_deserialization.py::test_overflow_atoms — the VM must error, not
// panic or hang, on an atom that claims more bytes than are present.
#[test]
fn vm_deserialize_overflow_atoms_error() {
    for size in [
        0xFFFF_FFFF_u64,
        0x3_FFFF_FFFF,
        0xFF_FFFF_FFFF,
        0x1FF_FFFF_FFFF,
    ] {
        let b = serialized_atom_overflow(size);
        assert!(
            run_deserialize(&b).is_err(),
            "over-large atom (size={size:#x}) must be rejected by the VM"
        );
    }
}

// test_compression.py::TestDecompression::test_deserialization
#[test]
fn vm_deserialize_hello_atom() {
    let b = Program::to("hello").serialized().unwrap().to_bytes();
    let out = run_deserialize(&b).unwrap();
    assert_eq!(out, Program::to("hello"));
}

// test_compression.py::TestDecompression::test_deserialization_as_argument —
// exercises the vendored test_generator_deserialize.clsp puzzle, which applies
// the passed-in deserializer to (list reserved_arg).
#[test]
fn vm_deserialize_as_generator_argument() {
    let test_gen = SerializedProgram::from_hex(TEST_GEN_DESERIALIZE_HEX.trim()).unwrap();
    let test_gen_prog = test_gen.to_program().unwrap();

    let de_serial = deserialize_mod();
    let de_prog = de_serial.to_program().unwrap();
    let de_sexp = de_prog.sexp().to_owned();

    let hello_bytes = Program::to("hello").serialized().unwrap().to_bytes();
    // args: (deserializer nil reserved_arg)
    let args = Program::new(SExp::from(vec![
        de_sexp,
        SExp::default(),
        SExp::from(hello_bytes),
    ]));
    let (_c, out) = test_gen_prog.run(INFINITE_COST, 0, &args).unwrap();
    assert_eq!(out, Program::to("hello"));
}

// test_generator_types.py::test_make_generator_args — the first argument to the
// block generator is the first template generator.
#[test]
fn make_generator_args_exposes_first_template() {
    let gen1 = hex::decode(GEN1_HEX).unwrap();
    // Program.to([[bytes(gen1)]])
    let gen_args = Program::new(SExp::from(vec![SExp::from(vec![SExp::from(gen1.clone())])]));
    let arg2 = gen_args.at("ff").unwrap();
    assert_eq!(arg2.as_vec(), Some(gen1));
}

// A real 121,543-byte mainnet-shaped generator must parse via the
// back-reference-aware decoder, consume exactly its own length (chia's
// `serialized_length` invariant), and round-trip to a stable tree.
#[test]
fn real_generator_bin_parses_and_round_trips() {
    let mut cursor = Cursor::new(CLVM_GENERATOR_BIN);
    let sexp = sexp_from_bytes_backrefs(&mut cursor).unwrap();
    assert!(matches!(sexp, SExp::Pair(_)));
    // serialized_length invariant: one program spans the whole blob.
    assert_eq!(cursor.position() as usize, CLVM_GENERATOR_BIN.len());

    // Re-serialize the expanded tree and re-parse: the tree is stable.
    let reserialized = sexp_to_bytes(&sexp).unwrap();
    let sexp2 = sexp_from_bytes(&mut Cursor::new(reserialized.as_ref())).unwrap();
    assert_eq!(sexp, sexp2);
    assert_eq!(sexp.tree_hash(), sexp2.tree_hash());
}

// The vendored multi-generator-input puzzle is a well-formed program that
// round-trips through the parser (fixture-load + decoder coverage).
#[test]
fn multiple_generator_input_puzzle_round_trips() {
    let serial = SerializedProgram::from_hex(TEST_MULTIPLE_HEX.trim()).unwrap();
    let bytes = serial.to_bytes();
    let sexp = sexp_from_bytes(&mut Cursor::new(bytes.as_slice())).unwrap();
    assert!(matches!(sexp, SExp::Pair(_)));
    let reserialized = sexp_to_bytes(&sexp).unwrap();
    assert_eq!(reserialized.as_ref(), bytes.as_slice());
}
