// Adversarial generator programs: a small CLVM source that, at RUN time, synthesizes a huge
// integer (via `concat`/`substr` ladders) or a very large number of conditions, probing the
// validator's cost/dedup bounds. Each program is assembled, wrapped in a simple-generator
// envelope, and driven through `execute_block_generator_result` (parse + aggregate) followed by
// `validate_block_conditions` / `validate_spend_context` — the same split the boundary suite in
// `block_generator.rs` uses.
//
// Pinned to the CURRENT-era regime (post soft-fork 9, the height the live node runs); the DoS
// bounds and condition errors under test do not change across forks.
//
// The bounds these vectors hold in place: the generator result must never be exported whole
// before it is bounded, or a program emitting a ~268 MB integer `num` times deep-copies that
// atom per condition and OOM-kills the node; integer arguments must be classified without
// copying; condition cost must be charged incrementally and bail at `MAX_BLOCK_COST_CLVM`; and
// duplicate CREATE_COIN must fail at the FIRST duplicate rather than materializing the whole
// list. The `timed` ceilings below are HW-independent safety valves for a superlinear
// regression, not speed measurements.

use dg_xch_core::blockchain::condition_opcode::ConditionOpcode;
use dg_xch_core::blockchain::spend_bundle_conditions::SpendBundleConditions;
use dg_xch_core::clvm::assemble::assemble_text;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, CoinSpendContext, ConditionValidationContext,
    execute_block_generator_result, validate_block_conditions,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::errors::ChiaError;
use std::collections::HashMap;
use std::time::{Duration, Instant};

// Post soft-fork 9 (mainnet 8,655,000): the regime the live node runs today.
const POST_SF9: u32 = 8_655_001;

// SINGLE_ARG_INT_LADDER_COND.
const SINGLE_ARG_INT_LADDER_COND: &str = "(a (q 2 4 (c 2 (c (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c (concat 5 11) ())) (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 24 {num})))";

// SINGLE_ARG_INT_COND.
const SINGLE_ARG_INT_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// SINGLE_ARG_INT_SUBSTR_COND.
const SINGLE_ARG_INT_SUBSTR_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 (q . 1)) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 28 {num})))";

// SINGLE_ARG_INT_SUBSTR_TAIL_COND.
const SINGLE_ARG_INT_SUBSTR_TAIL_COND: &str = "(a (q 2 4 (c 2 (c (concat (a 6 (c 2 (c (q . {filler}) (c 5 ())))) (q . {val})) (c 11 ())))) (c (q (a (i 11 (q 4 (c (q . {opcode}) (c 5 ())) (a 4 (c 2 (c (substr 5 () (- (strlen 5) (q . 1))) (c (- 11 (q . 1)) ()))))) ()) 1) 2 (i 11 (q 2 6 (c 2 (c (concat 5 5) (c (- 11 (q . 1)) ())))) (q . 5)) 1) (q 25 {num})))";

// CREATE_ANNOUNCE_COND.
const CREATE_ANNOUNCE_COND: &str = "(a (q 2 4 (c 2 (c (c (q . {opcode}) (c (a 6 (c 2 (c 5 ()))) ())) (c 11 ())))) (c (q (a (i 11 (q 4 5 (a 4 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) 23 (q . 97) 5) (q 8184 {num})))";

// CREATE_COIN: emits `num` identical CREATE_COIN conditions.
const CREATE_COIN: &str = "(a (q 2 2 (c 2 (c (q 51 \"abababababababababababababababab\" 1) (c 5 ())))) (c (q 2 (i 11 (q 4 5 (a 2 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// CREATE_UNIQUE_COINS: emits `num` CREATE_COIN, each a distinct amount.
const CREATE_UNIQUE_COINS: &str = "(a (q 2 6 (c 2 (c (q 51 \"abababababababababababababababab\") (c 5 ())))) (c (q (a (i 5 (q 4 9 (a 4 (c 2 (c 13 (c 11 ()))))) (q 4 11 ())) 1) 2 (i 11 (q 4 (a 4 (c 2 (c 5 (c 11 ())))) (a 6 (c 2 (c 5 (c (- 11 (q . 1)) ()))))) ()) 1) (q {num})))";

// The generator envelope: a simple generator returning ONE spend tuple (parent, puzzle_reveal,
// amount, solution) where the puzzle_reveal is the malicious program, evaluated with the empty
// solution `(() (q . ()))`.
fn build_generator(program_src: &str, coin_amount: u64) -> BlockGeneratorInput {
    let prg = format!(
        "(q ((0x0101010101010101010101010101010101010101010101010101010101010101 {program_src} {coin_amount} (() (q . ())))))"
    );
    let generator = assemble_text(&prg)
        .expect("malicious generator assembles")
        .serialized()
        .expect("serializes");
    BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: Vec::new(),
        constants: MAINNET,
        height: POST_SF9,
        flags: BlockGeneratorFlags {
            simple_generator: true,
            ..Default::default()
        },
    }
}

// A generous multiple of the reference runtimes. The tripwire only has to separate "seconds"
// from "OOM / minutes", so the slack keeps an unoptimized `cargo test` build — several times
// slower than release on the 280k/600k-condition programs — green while still catching a
// superlinear regression.
const DOS_CEILING_SLACK: u32 = 10;

// Run the generator under a wall-clock ceiling, widened by `DOS_CEILING_SLACK`.
fn timed<T>(reference: Duration, f: impl FnOnce() -> T) -> T {
    let ceiling = reference * DOS_CEILING_SLACK;
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    assert!(
        elapsed <= ceiling,
        "malicious generator exceeded {ceiling:?} (took {elapsed:?}, reference {reference:?}) — DoS bound regressed"
    );
    out
}

fn fmt(template: &str, opcode: u8, num: u64, val: &str, filler: &str) -> String {
    template
        .replace("{opcode}", &opcode.to_string())
        .replace("{num}", &num.to_string())
        .replace("{val}", val)
        .replace("{filler}", filler)
}

// The four time/height-lock opcodes the ladder vectors iterate over.
const LADDER_OPCODES: &[ConditionOpcode] = &[
    ConditionOpcode::AssertHeightAbsolute,
    ConditionOpcode::AssertHeightRelative,
    ConditionOpcode::AssertSecondsAbsolute,
    ConditionOpcode::AssertSecondsRelative,
];

fn expected_failure(opcode: ConditionOpcode) -> ChiaError {
    match opcode {
        ConditionOpcode::AssertHeightAbsolute => ChiaError::AssertHeightAbsoluteFailed,
        ConditionOpcode::AssertHeightRelative => ChiaError::AssertHeightRelativeFailed,
        ConditionOpcode::AssertSecondsAbsolute => ChiaError::AssertSecondsAbsoluteFailed,
        ConditionOpcode::AssertSecondsRelative => ChiaError::AssertSecondsRelativeFailed,
        ConditionOpcode::ReserveFee => ChiaError::ReserveFeeConditionFailed,
        _ => unreachable!("unexpected ladder opcode {opcode:?}"),
    }
}

// Validate the strict-collapsed condition against the block context. Absolute locks compare against
// the block height / previous-tx timestamp directly; relative locks compare against the spent coin's
// height / seconds, so a coin context is attached for the single spend. In every arm the huge
// asserted value exceeds the bound, so the lock's failure code is expected.
fn validate_ladder(
    opcode: ConditionOpcode,
    conds: &SpendBundleConditions,
) -> Result<(), ChiaError> {
    let mut ctx = ConditionValidationContext {
        block_height: POST_SF9,
        previous_transaction_block_timestamp: Some(1),
        coin_context: HashMap::new(),
    };
    if matches!(
        opcode,
        ConditionOpcode::AssertHeightRelative | ConditionOpcode::AssertSecondsRelative
    ) {
        ctx.coin_context.insert(
            conds.spends[0].coin_id,
            CoinSpendContext {
                birth_height: None,
                birth_seconds: None,
                spent_height: Some(0),
                spent_seconds: Some(0),
            },
        );
    }
    validate_block_conditions(conds, &ctx)
}

// ---- duplicate_large_integer_ladder ---------------------------------------------------------
// num=28 does not OOM, but it probes condition-arg byte-length sanitization: each asserted value
// is ~16 MB of leading zeros then a small counter. Stripping the pad down to the small counter
// would SATISFY the lock — an under-rejection. A leading-zero-padded value must be rejected
// outright (`buf.len() > 1 && buf[0] == 0 && (buf[1] & 0x80) == 0`); the pad classifies as
// `LeadingZero` and saturates the height to `u32::MAX`, so the lock fails.
#[test]
fn duplicate_large_integer_ladder() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_LADDER_COND, opcode as u8, 28, "", "0x00");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(1), || {
            execute_block_generator_result(&req)
        })
        .expect("ladder program parses to conditions");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode)),
            "opcode {opcode:?}"
        );
    }
}

// ---- duplicate_large_integer ----------------------------------------------------------------
// num=280,000 huge-integer args: deep-copying the shared ~268 MB atom once per condition OOMs the
// node, so each must be sanitize_uint-classified without a copy.
#[test]
fn duplicate_large_integer() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_COND, opcode as u8, 280_000, "100", "0x00");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(3), || {
            execute_block_generator_result(&req)
        })
        .expect("large-integer program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- duplicate_large_integer_substr ---------------------------------------------------------
// Same OOM class as `duplicate_large_integer`, substr variant (num=280,000). The arena substr is
// a zero-copy view, so the 280,000 whittled atoms cost O(1) each to classify.
#[test]
fn duplicate_large_integer_substr() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(
            SINGLE_ARG_INT_SUBSTR_COND,
            opcode as u8,
            280_000,
            "100",
            "0x00",
        );
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(2), || {
            execute_block_generator_result(&req)
        })
        .expect("substr program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- duplicate_large_integer_substr_tail ----------------------------------------------------
// num=280, each arg a large integer whittled from the tail by substr; same materialize-first OOM
// class, which the streaming parser bounds.
#[test]
fn duplicate_large_integer_substr_tail() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(
            SINGLE_ARG_INT_SUBSTR_TAIL_COND,
            opcode as u8,
            280,
            "0xffffffff",
            "0x00",
        );
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(1), || {
            execute_block_generator_result(&req)
        })
        .expect("substr-tail program parses");
        assert_eq!(
            validate_ladder(opcode, &conds),
            Err(expected_failure(opcode))
        );
    }
}

// ---- duplicate_large_integer_negative -------------------------------------------------------
// A negative filler (0xff) makes every asserted value negative, hence a no-op, so the block is
// ACCEPTED with exactly one spend. num=280,000 huge negative-integer args OOM the node if they
// are materialized before the no-op collapse; sanitize_uint reads only the sign byte.
#[test]
fn duplicate_large_integer_negative() {
    for &opcode in LADDER_OPCODES {
        let src = fmt(SINGLE_ARG_INT_COND, opcode as u8, 280_000, "100", "0xff");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_millis(2750), || {
            execute_block_generator_result(&req)
        })
        .expect("negative program parses");
        assert!(
            validate_ladder(opcode, &conds).is_ok(),
            "opcode {opcode:?}: a negative time/height lock is a no-op, not a failure"
        );
        assert_eq!(conds.spends.len(), 1);
    }
}

// ---- duplicate_reserve_fee ------------------------------------------------------------------
// num=280,000 huge reserve-fee args: parse_amount(8) must fail the first oversized fee outright
// rather than materializing them all.
#[test]
fn duplicate_reserve_fee() {
    let src = fmt(
        SINGLE_ARG_INT_COND,
        ConditionOpcode::ReserveFee as u8,
        280_000,
        "100",
        "0x00",
    );
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // A reserve fee this size cannot be paid.
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- duplicate_reserve_fee_negative ---------------------------------------------------------
// num=200,000 huge negative reserve-fee args, the same OOM class.
#[test]
fn duplicate_reserve_fee_negative() {
    let src = fmt(
        SINGLE_ARG_INT_COND,
        ConditionOpcode::ReserveFee as u8,
        200_000,
        "100",
        "0xff",
    );
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // A negative RESERVE_FEE fails unconditionally.
    assert_eq!(result, Err(ChiaError::ReserveFeeConditionFailed));
}

// ---- duplicate_coin_announces ---------------------------------------------------------------
// 1024 announcements per spend: the 1024 cap is a MEMPOOL rule, so CONSENSUS validation ACCEPTS
// this. `validate_block_conditions` must not impose an announcement-count cap.
#[test]
fn duplicate_coin_announces() {
    for opcode in [
        ConditionOpcode::CreateCoinAnnouncement,
        ConditionOpcode::CreatePuzzleAnnouncement,
    ] {
        let src = fmt(CREATE_ANNOUNCE_COND, opcode as u8, 1024, "", "");
        let req = build_generator(&src, 123);
        let conds = timed(Duration::from_secs(14), || {
            execute_block_generator_result(&req)
        })
        .expect("1024 announcements are accepted in consensus mode");
        let ctx = ConditionValidationContext {
            block_height: POST_SF9,
            previous_transaction_block_timestamp: None,
            coin_context: HashMap::new(),
        };
        assert!(validate_block_conditions(&conds, &ctx).is_ok());
        assert_eq!(conds.spends.len(), 1);
    }
}

// ---- create_coin_duplicates -----------------------------------------------------------------
// num=600,000 identical CREATE_COINs. Materializing them builds a 600k-deep condition list whose
// owned-SExp teardown overflows the native stack, so the parser must fail at the first duplicate
// (coin #2) and never build the list.
#[test]
fn create_coin_duplicates() {
    let src = CREATE_COIN.replace("{num}", "600000");
    let req = build_generator(&src, 123);
    let result = timed(Duration::from_millis(1500), || {
        execute_block_generator_result(&req)
    });
    // DUPLICATE_OUTPUT, failing at the first duplicate.
    assert_eq!(result, Err(ChiaError::DuplicateOutput));
}

// ---- many_create_coin -----------------------------------------------------------------------
#[test]
fn many_create_coin() {
    let src = CREATE_UNIQUE_COINS.replace("{num}", "6094");
    let req = build_generator(&src, 123_000_000);
    let conds = timed(Duration::from_millis(300), || {
        execute_block_generator_result(&req)
    })
    .expect("6094 distinct create-coins are accepted");
    assert_eq!(conds.spends.len(), 1);
    assert_eq!(conds.spends[0].create_coin.len(), 6094);
}
