use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};
use dg_xch_core::clvm::utils::{
    CANONICAL_INTS, COST_CONDITIONS, DISABLE_OP, ENABLE_KECCAK_OPS_OUTSIDE_FORK, LIMITS,
    MEMPOOL_MODE, NEW_COST_MODEL, RELAXED_BLS,
};
use std::collections::BTreeMap;

const GOLDEN_PATH: &str = "tests/fixtures/clvm_random_differential.json";
const PROGRAMS: u64 = 400;

/// xorshift64*. Deterministic and dependency-free so the generated corpus is a stable artifact
/// rather than something that drifts with a crate upgrade.
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
        (self.next() % n as u64) as usize
    }
    fn pick<'t, T>(&mut self, xs: &'t [T]) -> &'t T {
        &xs[self.below(xs.len())]
    }
}

/// What an operator wants in an argument position. Supplying the right kind is what makes a
/// generated program run deep instead of erroring on its first argument check.
#[derive(Copy, Clone)]
enum Kind {
    /// An integer small enough to exercise arithmetic without dominating cost.
    SmallInt,
    /// A byte string, including the encodings that have historically been mishandled.
    Bytes,
    /// Exactly 32 bytes — what coinid and the hash-shaped operators expect.
    Bytes32,
    /// Nil or non-nil; drives the boolean and control operators.
    Bool,
    /// Any subtree at all, atom or pair.
    Tree,
    /// A proper list, for the operators that walk one.
    List,
}

/// Operators exercised by generation, with the argument kinds each wants. Excludes the BLS group
/// operators: random generation cannot reach valid curve points, so they would only produce parse
/// errors.
const OPS: &[(&str, u8, &[Kind])] = &[
    ("if", 3, &[Kind::Bool, Kind::Tree, Kind::Tree]),
    ("cons", 4, &[Kind::Tree, Kind::Tree]),
    ("first", 5, &[Kind::List]),
    ("rest", 6, &[Kind::List]),
    ("listp", 7, &[Kind::Tree]),
    ("eq", 9, &[Kind::Bytes, Kind::Bytes]),
    ("gr_bytes", 10, &[Kind::Bytes, Kind::Bytes]),
    ("sha256", 11, &[Kind::Bytes, Kind::Bytes]),
    ("substr", 12, &[Kind::Bytes, Kind::SmallInt, Kind::SmallInt]),
    ("strlen", 13, &[Kind::Bytes]),
    ("concat", 14, &[Kind::Bytes, Kind::Bytes]),
    ("add", 16, &[Kind::SmallInt, Kind::SmallInt]),
    ("subtract", 17, &[Kind::SmallInt, Kind::SmallInt]),
    ("multiply", 18, &[Kind::SmallInt, Kind::SmallInt]),
    ("div", 19, &[Kind::SmallInt, Kind::SmallInt]),
    ("divmod", 20, &[Kind::SmallInt, Kind::SmallInt]),
    ("gr", 21, &[Kind::SmallInt, Kind::SmallInt]),
    ("ash", 22, &[Kind::SmallInt, Kind::SmallInt]),
    ("lsh", 23, &[Kind::SmallInt, Kind::SmallInt]),
    ("logand", 24, &[Kind::SmallInt, Kind::SmallInt]),
    ("logior", 25, &[Kind::SmallInt, Kind::SmallInt]),
    ("logxor", 26, &[Kind::SmallInt, Kind::SmallInt]),
    ("lognot", 27, &[Kind::SmallInt]),
    ("not", 32, &[Kind::Bool]),
    ("any", 33, &[Kind::Bool, Kind::Bool]),
    ("all", 34, &[Kind::Bool, Kind::Bool]),
    (
        "coinid",
        48,
        &[Kind::Bytes32, Kind::Bytes32, Kind::SmallInt],
    ),
    (
        "modpow",
        60,
        &[Kind::SmallInt, Kind::SmallInt, Kind::SmallInt],
    ),
    ("mod", 61, &[Kind::SmallInt, Kind::SmallInt]),
];

/// Integer literals biased toward the values that have historically broken things: sign
/// boundaries, canonical-encoding edges, shift limits, and zero.
const INT_EDGES: &[i64] = &[
    0,
    1,
    -1,
    2,
    -2,
    3,
    7,
    8,
    255,
    256,
    -256,
    127,
    128,
    -128,
    65535,
    65536,
    -65536,
    1_000_000,
    -1_000_000,
    i32::MAX as i64,
    i32::MIN as i64,
];

fn atom(bytes: Vec<u8>) -> SExp<'static> {
    SExp::Atom(AtomBuf::new(bytes))
}

/// `(q . v)` — a DOTTED pair. Building `(q v)` instead yields a two-element list whose quoted
/// value is `(v)`, so every argument would arrive wrapped in an extra list level and fail its
/// type check.
fn quoted(v: SExp<'static>) -> SExp<'static> {
    SExp::Pair(dg_xch_core::clvm::sexp::PairBuf::from((atom(vec![1]), v)))
}

fn gen_bytes(rng: &mut Rng) -> Vec<u8> {
    match rng.below(8) {
        // The encodings that have caused trouble: empty, a bare zero, a leading zero before a
        // high-bit byte (canonical) and one that is not, and 0x80.
        0 => vec![],
        1 => vec![0],
        2 => vec![0, 0x80],
        3 => vec![0, 0x01],
        4 => vec![0x80],
        5 => vec![0xff],
        6 => (0..rng.below(8) + 1).map(|_| rng.next() as u8).collect(),
        _ => (0..rng.below(40) + 1).map(|_| rng.next() as u8).collect(),
    }
}

/// Returns the value and whether it is already an expression (a call) rather than a literal
/// that must be quoted. Inferring this from the shape is wrong: a literal cons cell is a pair too.
fn gen_value(rng: &mut Rng, kind: Kind, depth: u32) -> (SExp<'static>, bool) {
    // At depth, stop nesting and emit a literal so programs stay bounded.
    let nest = depth > 0 && rng.below(4) == 0;
    match kind {
        Kind::SmallInt => {
            if nest {
                return (gen_call(rng, depth - 1), true);
            }
            let v = *rng.pick(INT_EDGES);
            if v == 0 {
                (atom(vec![]), false)
            } else {
                let mut b = v.to_be_bytes().to_vec();
                while b.len() > 1
                    && ((b[0] == 0 && b[1] & 0x80 == 0) || (b[0] == 0xff && b[1] & 0x80 != 0))
                {
                    b.remove(0);
                }
                (atom(b), false)
            }
        }
        Kind::Bytes => {
            if nest {
                return (gen_call(rng, depth - 1), true);
            }
            (atom(gen_bytes(rng)), false)
        }
        Kind::Bytes32 => (atom((0..32).map(|_| rng.next() as u8).collect()), false),
        Kind::Bool => {
            if nest {
                return (gen_call(rng, depth - 1), true);
            }
            (
                if rng.below(2) == 0 {
                    atom(vec![])
                } else {
                    atom(vec![1])
                },
                false,
            )
        }
        Kind::Tree => {
            if nest {
                return (gen_call(rng, depth - 1), true);
            }
            if depth > 0 && rng.below(3) == 0 {
                let a = gen_value(rng, Kind::Bytes, depth - 1).0;
                let b = gen_value(rng, Kind::Bytes, depth - 1).0;
                (SExp::from(vec![a, b]), false)
            } else {
                (atom(gen_bytes(rng)), false)
            }
        }
        Kind::List => {
            let n = rng.below(4);
            let items = (0..n)
                .map(|_| gen_value(rng, Kind::Bytes, depth.saturating_sub(1)).0)
                .collect::<Vec<_>>();
            (SExp::from(items), false)
        }
    }
}

/// One operator application with type-appropriate arguments, each either a quoted literal or a
/// nested call producing the same kind.
fn gen_call(rng: &mut Rng, depth: u32) -> SExp<'static> {
    let (_, opcode, kinds) = *rng.pick(OPS);
    let mut items = vec![atom(vec![opcode])];
    for kind in kinds {
        let (v, is_call) = gen_value(rng, *kind, depth);
        // A nested call is already an expression; a literal must be quoted or the VM would try to
        // evaluate it as a program.
        items.push(if is_call { v } else { quoted(v) });
    }
    SExp::from(items)
}

fn outcome(program: &SExp, flags: u32) -> String {
    let nil = SExp::Atom(AtomBuf::new(vec![]));
    let mut runtime = ClvmRuntime::new(u64::MAX, flags);
    match runtime.run(program, &nil) {
        Ok((cost, out)) => format!("ok {cost} {out}"),
        Err(e) => {
            // Error text carries operator-level detail; keep it bounded so one pathological
            // message cannot dominate the golden.
            let mut s = format!("err {e:?}");
            s.truncate(160);
            s
        }
    }
}

fn collect() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // The dialect configurations that actually occur on chain, in the order the forks activate
    // (`BlockGeneratorFlags::for_height`), plus mempool mode. Sweeping the real ladder rather than
    // an arbitrary flag matrix is what makes a divergence here mean something: every one of these
    // is a live consensus regime, and the boundaries between them are where divergence has
    // historically hidden.
    const HARD_FORK: u32 = COST_CONDITIONS | ENABLE_KECCAK_OPS_OUTSIDE_FORK;
    const SOFT_FORK8: u32 = HARD_FORK | DISABLE_OP | LIMITS;
    const SOFT_FORK9: u32 = SOFT_FORK8 | CANONICAL_INTS;
    const HARD_FORK2: u32 = SOFT_FORK9 | NEW_COST_MODEL | RELAXED_BLS;
    let ladder: [(&str, u32); 6] = [
        ("genesis", 0),
        ("hardfork", HARD_FORK),
        ("softfork8", SOFT_FORK8),
        ("softfork9", SOFT_FORK9),
        ("hardfork2", HARD_FORK2),
        ("mempool", MEMPOOL_MODE),
    ];
    for (mode, flags) in ladder {
        for seed in 0..PROGRAMS {
            let mut rng = Rng::new(seed ^ 0x5EED_1234);
            let program = gen_call(&mut rng, 3);
            out.insert(format!("{mode}/{seed:04}"), outcome(&program, flags));
        }
    }
    out
}

#[test]
fn generated_programs_match_golden_in_every_flag_mode() {
    let all = collect();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(
            GOLDEN_PATH,
            serde_json::to_string_pretty(&all).expect("serializes"),
        )
        .expect("golden writes");
        eprintln!("  wrote {} generated outcomes to {GOLDEN_PATH}", all.len());
        return;
    }

    let stored = std::fs::read_to_string(GOLDEN_PATH)
        .expect("golden missing — harvest once with UPDATE_GOLDEN=1");
    let stored: BTreeMap<String, String> = serde_json::from_str(&stored).expect("golden parses");

    let mut diverged = Vec::new();
    for (name, got) in &all {
        match stored.get(name) {
            Some(want) if want == got => {}
            Some(want) => diverged.push(format!("  {name}\n    want: {want}\n    got:  {got}")),
            None => panic!("{name}: no golden entry — harvest with UPDATE_GOLDEN=1"),
        }
    }
    assert!(
        diverged.is_empty(),
        "{} of {} generated programs diverged from pinned behavior:\n{}\n\
         Each name is mode/seed; the seed reproduces the exact program.",
        diverged.len(),
        all.len(),
        diverged
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        all.len(),
        stored.len(),
        "golden holds entries no seed produces"
    );
    eprintln!(
        "  {} generated programs hold across both flag modes",
        all.len()
    );
}

#[test]
fn generation_is_deterministic_and_reaches_real_execution() {
    // A corpus that mostly fails to run would pin nothing. Require that a solid majority of
    // generated programs actually execute, so the golden reflects VM behavior rather than
    // argument-check rejections.
    let mut ok = 0usize;
    for seed in 0..PROGRAMS {
        let mut a = Rng::new(seed ^ 0x5EED_1234);
        let mut b = Rng::new(seed ^ 0x5EED_1234);
        let (pa, pb) = (gen_call(&mut a, 3), gen_call(&mut b, 3));
        assert_eq!(
            format!("{pa}"),
            format!("{pb}"),
            "seed {seed}: generation is not deterministic"
        );
        if outcome(&pa, 0).starts_with("ok ") {
            ok += 1;
        }
    }
    let pct = ok * 100 / PROGRAMS as usize;
    eprintln!("  {ok}/{PROGRAMS} generated programs execute successfully ({pct}%)");
    assert!(
        pct >= 50,
        "only {pct}% of generated programs run; type-directed generation has regressed and the \
         corpus is mostly argument-check rejections"
    );
}
