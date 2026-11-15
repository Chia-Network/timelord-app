// Parser robustness against arbitrary bytes.
//
// Every serialized CLVM program the node parses arrives from a peer, so the decoder's input is
// attacker-controlled in full. The requirement is narrow and absolute: for ANY byte string it
// returns `Ok` or `Err`. It must not panic, must not abort, and must not allocate on the strength
// of a length the input merely claims.
//
// That last clause is the one with history behind it: a decoder that trusts a declared atom length
// turns seven bytes of input into a multi-gigabyte allocation, and a peer can send it for free.
// `clvm_adversarial_limits.rs` pins the specific oversized-header cases; this covers the space
// around them, where the malformed input is structurally plausible rather than obviously hostile —
// truncated mid-atom, a pair whose tail is missing, a length prefix one byte short of its payload.
//
// The corpus is generated from a seeded PRNG (no dependencies), so it is identical on every machine
// and a failure names the seed that reproduces it. Structure-aware mutation matters more than
// volume here: uniformly random bytes are rejected in the first few bytes and never reach the
// interesting paths, so most cases start from a well-formed program and corrupt it.

use dg_xch_core::clvm::program::SerializedProgram;
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A well-formed serialized program to corrupt. Built through the real serializer so the starting
/// point is genuinely valid rather than a hand-written guess at the encoding.
fn seed_program(rng: &mut Rng) -> Vec<u8> {
    fn build(rng: &mut Rng, depth: u32) -> SExp<'static> {
        if depth == 0 || rng.below(3) == 0 {
            let n = rng.below(40);
            SExp::Atom(AtomBuf::new((0..n).map(|_| rng.next() as u8).collect()))
        } else {
            SExp::from(vec![build(rng, depth - 1), build(rng, depth - 1)])
        }
    }
    let sexp = build(rng, 4);
    dg_xch_core::clvm::parser::sexp_to_bytes(&sexp)
        .expect("serializer round-trips its own tree")
        .as_ref()
        .to_vec()
}

/// Corruptions that keep the input structurally plausible — the shapes a decoder is most likely to
/// mishandle, as opposed to noise it rejects immediately.
fn corrupt(rng: &mut Rng, mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.is_empty() {
        return bytes;
    }
    match rng.below(7) {
        // Truncate: the classic "claims more than it carries".
        0 => {
            let keep = rng.below(bytes.len());
            bytes.truncate(keep);
        }
        // Flip one byte: may turn an atom header into a pair marker or inflate a length.
        1 => {
            let i = rng.below(bytes.len());
            bytes[i] ^= 1 << rng.below(8);
        }
        // Overwrite a byte with a structurally meaningful one.
        2 => {
            let i = rng.below(bytes.len());
            const MARKERS: [u8; 5] = [0xff, 0x80, 0xfe, 0xfc, 0x00];
            bytes[i] = MARKERS[rng.below(MARKERS.len())];
        }
        // Splice in a large declared length without the bytes to back it.
        3 => {
            let i = rng.below(bytes.len());
            const BIG_LEN: [u8; 4] = [0xfc, 0xff, 0xff, 0xff];
            bytes.splice(i..i, BIG_LEN);
        }
        // Extra trailing bytes after a complete program.
        4 => bytes.extend((0..rng.below(8) + 1).map(|_| rng.next() as u8)),
        // Deeply nested pair prefix with no payload to satisfy it.
        5 => {
            let depth = rng.below(2000) + 1;
            let mut out = vec![0xff; depth];
            out.extend(bytes);
            bytes = out;
        }
        // Drop a byte from the middle.
        _ => {
            let i = rng.below(bytes.len());
            bytes.remove(i);
        }
    }
    bytes
}

#[test]
fn arbitrary_bytes_never_panic_the_parser() {
    // The whole property: Ok or Err, never a panic and never an abort. A panic here fails the test
    // by unwinding; a stack overflow or OOM would take the process down, which is precisely the
    // outcome being excluded.
    const CASES: u64 = 4000;
    let mut ok = 0usize;
    let mut err = 0usize;

    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0xC0FF_EE00);
        let bytes = if seed % 8 == 0 {
            // A minority of uniformly random inputs, for the shapes corruption cannot reach.
            let n = rng.below(64);
            (0..n).map(|_| rng.next() as u8).collect()
        } else {
            let base = seed_program(&mut rng);
            corrupt(&mut rng, base)
        };

        match SerializedProgram::from(bytes.clone()).to_program() {
            Ok(_) => ok += 1,
            Err(_) => err += 1,
        }
    }

    eprintln!("  {CASES} malformed inputs parsed: {ok} accepted, {err} rejected, 0 panics");
    // Both outcomes must occur, otherwise the corpus is degenerate — all-rejected would mean the
    // inputs never reach the decoder's interesting paths.
    assert!(
        ok > 0,
        "no generated input parsed; the corpus is not reaching the decoder"
    );
    assert!(
        err > 0,
        "every generated input parsed; corruption is not producing invalid encodings"
    );
}

#[test]
fn a_parsed_program_reserializes_to_the_same_bytes() {
    // Round-trip identity on the inputs that DO parse. A decoder that accepts a program but
    // reconstructs it differently changes the program's identity hash, which in consensus terms is
    // a different program entirely — a silent failure no panic-freedom test would catch.
    const CASES: u64 = 1500;
    let mut checked = 0usize;

    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0xBEEF_0001);
        let bytes = seed_program(&mut rng);
        // `to_program` borrows the SerializedProgram, so it has to outlive the parsed program.
        let serialized = SerializedProgram::from(bytes.clone());
        let Ok(program) = serialized.to_program() else {
            continue;
        };
        let round = dg_xch_core::clvm::parser::sexp_to_bytes(program.sexp())
            .expect("a parsed program reserializes")
            .as_ref()
            .to_vec();
        assert_eq!(
            round, bytes,
            "seed {seed}: parse->serialize changed the bytes, so the program's identity changed"
        );
        checked += 1;
    }

    eprintln!("  {checked}/{CASES} well-formed programs round-tripped byte-identically");
    assert!(
        checked > CASES as usize / 2,
        "too few programs round-tripped to be meaningful"
    );
}
