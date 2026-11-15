//! CLVM BLS operators — opcodes 49..=59, dispatched unconditionally in the base dialect
//! (the 2.0 hard fork made the BLS extension available outside the `softfork` guard).
//! `bls_pairing_identity`/`bls_verify` must RAISE on a failed check.

use crate::clvm::arena::{Arena, NodePtr};
use crate::clvm::dialect::Dialect;
use crate::clvm::pure_ops::OpOut;
use crate::errors::ClvmError;

#[cfg(feature = "bls")]
use crate::clvm::more_ops::{MALLOC_COST_PER_BYTE, bls_g1, mod_group_order, new_atom_and_cost};
#[cfg(feature = "bls")]
use crate::clvm::utils::{LIMITS, RELAXED_BLS, atom, check_cost, int_atom, split};
#[cfg(feature = "bls")]
use crate::formatting::number_from_slice;

// cost constants
#[cfg(feature = "bls")]
const BLS_G1_SUBTRACT_BASE_COST: u64 = 101_094;
#[cfg(feature = "bls")]
const BLS_G1_SUBTRACT_COST_PER_ARG: u64 = 1_343_980;
#[cfg(feature = "bls")]
const BLS_G1_MULTIPLY_BASE_COST: u64 = 705_500;
#[cfg(feature = "bls")]
const BLS_G1_MULTIPLY_COST_PER_BYTE: u64 = 10;
#[cfg(feature = "bls")]
const BLS_G1_NEGATE_BASE_COST: u64 = 1396 - 480;
#[cfg(feature = "bls")]
const BLS_G2_ADD_BASE_COST: u64 = 80_000;
#[cfg(feature = "bls")]
const BLS_G2_ADD_COST_PER_ARG: u64 = 1_950_000;
#[cfg(feature = "bls")]
const BLS_G2_SUBTRACT_BASE_COST: u64 = 80_000;
#[cfg(feature = "bls")]
const BLS_G2_SUBTRACT_COST_PER_ARG: u64 = 1_950_000;
#[cfg(feature = "bls")]
const BLS_G2_MULTIPLY_BASE_COST: u64 = 2_100_000;
#[cfg(feature = "bls")]
const BLS_G2_MULTIPLY_COST_PER_BYTE: u64 = 5;
#[cfg(feature = "bls")]
const BLS_G2_NEGATE_BASE_COST: u64 = 2164 - 960;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G1_BASE_COST: u64 = 195_000;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G1_COST_PER_BYTE: u64 = 4;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G1_COST_PER_DST_BYTE: u64 = 4;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G2_BASE_COST: u64 = 815_000;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G2_COST_PER_BYTE: u64 = 4;
#[cfg(feature = "bls")]
const BLS_MAP_TO_G2_COST_PER_DST_BYTE: u64 = 4;
#[cfg(feature = "bls")]
const BLS_PAIRING_BASE_COST: u64 = 3_000_000;
#[cfg(feature = "bls")]
const BLS_PAIRING_COST_PER_ARG: u64 = 1_200_000;

#[cfg(feature = "bls")]
const DST_G1: &[u8; 43] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_AUG_";
#[cfg(feature = "bls")]
const DST_G2: &[u8; 43] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_AUG_";

/// G2 primitives over raw `blst` FFI: parse/validate (infinity accepted), projective
/// add/sub/negate/scalar-multiply, compressed encoding, hash-to-curve, and the two pairing
/// verifiers the pairing operators call. Points are held projective (`blst_p2`).
#[cfg(feature = "bls")]
mod bls_g2 {
    use blst::{
        BLST_ERROR, blst_aggregated_in_g2, blst_fp12, blst_hash_to_g2, blst_p1, blst_p1_affine,
        blst_p1_to_affine, blst_p2, blst_p2_add_or_double, blst_p2_affine, blst_p2_cneg,
        blst_p2_compress, blst_p2_from_affine, blst_p2_in_g2, blst_p2_is_inf, blst_p2_mult,
        blst_p2_to_affine, blst_p2_uncompress, blst_pairing, blst_pairing_aggregate_pk_in_g1,
        blst_pairing_commit, blst_pairing_finalverify, blst_pairing_init,
        blst_pairing_raw_aggregate, blst_pairing_sizeof, blst_scalar, blst_scalar_from_be_bytes,
    };
    use std::mem::MaybeUninit;

    /// The point at infinity (all-zero projective point).
    pub(super) fn identity() -> blst_p2 {
        blst_p2::default()
    }

    /// Parse a 96-byte compressed G2 element: `blst_p2_uncompress` (flag/encoding
    /// validation), then `is_valid` = infinity OR in-subgroup. `None` on any failure.
    pub(super) fn parse_g2_compressed(bytes: &[u8; 96]) -> Option<blst_p2> {
        // SAFETY: `bytes` is a valid 96-byte buffer; on BLST_SUCCESS `affine` is initialized,
        // and `blst_p2_from_affine` fully initializes `p2` from it.
        let p2 = unsafe {
            let mut affine = MaybeUninit::<blst_p2_affine>::uninit();
            if blst_p2_uncompress(affine.as_mut_ptr(), bytes.as_ptr()) != BLST_ERROR::BLST_SUCCESS {
                return None;
            }
            let mut p2 = MaybeUninit::<blst_p2>::uninit();
            blst_p2_from_affine(p2.as_mut_ptr(), affine.as_ptr());
            p2.assume_init()
        };
        if is_valid(&p2) { Some(p2) } else { None }
    }

    /// Infinity OR subgroup member.
    pub(super) fn is_valid(p: &blst_p2) -> bool {
        // SAFETY: `p` is a valid initialized point.
        unsafe { blst_p2_is_inf(&raw const *p) || blst_p2_in_g2(&raw const *p) }
    }

    pub(super) fn is_inf(p: &blst_p2) -> bool {
        // SAFETY: `p` is a valid initialized point.
        unsafe { blst_p2_is_inf(&raw const *p) }
    }

    /// `acc += p` (`blst_p2_add_or_double`).
    pub(super) fn add_assign(acc: &mut blst_p2, p: &blst_p2) {
        // SAFETY: both operands are valid initialized points.
        unsafe {
            blst_p2_add_or_double(&raw mut *acc, &raw const *acc, &raw const *p);
        }
    }

    /// `acc -= p` (negate a copy, then add-or-double).
    pub(super) fn sub_assign(acc: &mut blst_p2, p: &blst_p2) {
        // SAFETY: both operands are valid initialized points; `neg` is a local copy.
        unsafe {
            let mut neg = *p;
            blst_p2_cneg(&raw mut neg, true);
            blst_p2_add_or_double(&raw mut *acc, &raw const *acc, &raw const neg);
        }
    }

    /// `p *= n` for a big-endian, group-order-reduced, non-negative integer
    /// (256-bit blst scalar width).
    pub(super) fn scalar_multiply(p: &mut blst_p2, n_be: &[u8]) {
        debug_assert!(!n_be.is_empty() && n_be.len() <= 32);
        // SAFETY: `blst_scalar_from_be_bytes` fully initializes `scalar` for 1..=32 input
        // bytes; `blst_p2_mult` reads 256 scalar bits and writes a fully-initialized point.
        unsafe {
            let mut scalar = MaybeUninit::<blst_scalar>::uninit();
            blst_scalar_from_be_bytes(scalar.as_mut_ptr(), n_be.as_ptr(), n_be.len());
            blst_p2_mult(
                &raw mut *p,
                &raw const *p,
                scalar.as_ptr().cast::<u8>(),
                256,
            );
        }
    }

    /// Compressed encoding via `blst_p2_compress`; infinity encodes as `0xc0 || 0^95`.
    pub(super) fn to_compressed(p: &blst_p2) -> [u8; 96] {
        // SAFETY: `p` is a valid initialized point; `blst_p2_compress` writes all 96 bytes.
        unsafe {
            let mut bytes = MaybeUninit::<[u8; 96]>::uninit();
            blst_p2_compress(bytes.as_mut_ptr().cast::<u8>(), &raw const *p);
            bytes.assume_init()
        }
    }

    /// Hash to G2 (`blst_hash_to_g2`, empty aug).
    pub(super) fn hash_to_g2(msg: &[u8], dst: &[u8]) -> blst_p2 {
        // SAFETY: all pointers are valid for their lengths; `blst_hash_to_g2` fully
        // initializes the output point.
        unsafe {
            let mut p2 = MaybeUninit::<blst_p2>::uninit();
            blst_hash_to_g2(
                p2.as_mut_ptr(),
                msg.as_ptr(),
                msg.len(),
                dst.as_ptr(),
                dst.len(),
                std::ptr::null(),
                0,
            );
            p2.assume_init()
        }
    }

    /// Raw-aggregate every `(G1, G2)` pair into one pairing context and final-verify
    /// against the identity. Per-item `is_valid` re-checks are elided: every point here
    /// was parsed by the strict operator argument path, which already enforced them.
    pub(super) fn aggregate_pairing(items: &[(blst_p1, blst_p2)]) -> bool {
        if items.is_empty() {
            return true;
        }
        // SAFETY: `v` is sized by `blst_pairing_sizeof`; the context pointer stays valid for
        // the whole call; affine conversions read valid initialized points.
        unsafe {
            let mut v: Vec<u64> = vec![0; blst_pairing_sizeof() / 8];
            let ctx = v.as_mut_slice().as_mut_ptr().cast::<blst_pairing>();
            blst_pairing_init(ctx, true, super::DST_G2.as_ptr(), super::DST_G2.len());
            for (g1, g2) in items {
                let mut g1_affine = MaybeUninit::<blst_p1_affine>::uninit();
                blst_p1_to_affine(g1_affine.as_mut_ptr(), &raw const *g1);
                let mut g2_affine = MaybeUninit::<blst_p2_affine>::uninit();
                blst_p2_to_affine(g2_affine.as_mut_ptr(), &raw const *g2);
                blst_pairing_raw_aggregate(ctx, g2_affine.as_ptr(), g1_affine.as_ptr());
            }
            blst_pairing_commit(ctx);
            blst_pairing_finalverify(ctx, std::ptr::null())
        }
    }

    /// AUG-scheme verify of `sig` against `(pk, msg)` pairs — each message is prepended
    /// with its public key's compressed bytes and aggregated under the G2 AUG DST. The
    /// empty set verifies iff the signature is the identity. Per-item `is_valid` re-checks
    /// elided as above.
    pub(super) fn aggregate_verify(sig: &blst_p2, items: &[(blst_p1, Vec<u8>)]) -> bool {
        if items.is_empty() {
            return is_inf(sig);
        }
        // SAFETY: as in `aggregate_pairing`; `blst_aggregated_in_g2` reads a valid affine
        // signature and writes a fully-initialized fp12.
        unsafe {
            let mut sig_affine = MaybeUninit::<blst_p2_affine>::uninit();
            let mut sig_gt = MaybeUninit::<blst_fp12>::uninit();
            blst_p2_to_affine(sig_affine.as_mut_ptr(), &raw const *sig);
            blst_aggregated_in_g2(sig_gt.as_mut_ptr(), sig_affine.as_ptr());
            let sig_gt = sig_gt.assume_init();

            let mut v: Vec<u64> = vec![0; blst_pairing_sizeof() / 8];
            let ctx = v.as_mut_slice().as_mut_ptr().cast::<blst_pairing>();
            blst_pairing_init(ctx, true, super::DST_G2.as_ptr(), super::DST_G2.len());
            let mut aug_msg = Vec::<u8>::new();
            for (pk, msg) in items {
                let mut pk_affine = MaybeUninit::<blst_p1_affine>::uninit();
                blst_p1_to_affine(pk_affine.as_mut_ptr(), &raw const *pk);

                aug_msg.clear();
                aug_msg.extend_from_slice(&super::g1_to_compressed_projective(pk));
                aug_msg.extend_from_slice(msg);

                if blst_pairing_aggregate_pk_in_g1(
                    ctx,
                    pk_affine.as_ptr(),
                    std::ptr::null(),
                    aug_msg.as_ptr(),
                    aug_msg.len(),
                    std::ptr::null(),
                    0,
                ) != BLST_ERROR::BLST_SUCCESS
                {
                    return false;
                }
            }
            blst_pairing_commit(ctx);
            blst_pairing_finalverify(ctx, &raw const sig_gt)
        }
    }
}

/// Compressed encoding of a projective G1 point.
#[cfg(feature = "bls")]
fn g1_to_compressed_projective(p: &blst::blst_p1) -> [u8; 48] {
    bls_g1::to_compressed(p)
}

/// Affine → projective — the parsed argument form (`bls_g1::parse_g1_compressed` returns
/// affine) lifted into the arithmetic form.
#[cfg(feature = "bls")]
fn g1_projective(p: &blst::blst_p1_affine) -> blst::blst_p1 {
    // SAFETY: `p` is a valid initialized affine point; `blst_p1_from_affine` fully
    // initializes the output.
    unsafe {
        let mut out = std::mem::MaybeUninit::<blst::blst_p1>::uninit();
        blst::blst_p1_from_affine(out.as_mut_ptr(), &raw const *p);
        out.assume_init()
    }
}

/// `acc -= p` on projective G1 (negate a copy, add-or-double).
#[cfg(feature = "bls")]
fn g1_sub_assign(acc: &mut blst::blst_p1, p: &blst::blst_p1) {
    // SAFETY: both operands valid initialized points; `neg` is a local copy.
    unsafe {
        let mut neg = *p;
        blst::blst_p1_cneg(&raw mut neg, true);
        blst::blst_p1_add_or_double(&raw mut *acc, &raw const *acc, &raw const neg);
    }
}

/// `p *= n` for a big-endian, group-order-reduced integer.
#[cfg(feature = "bls")]
fn g1_scalar_multiply(p: &mut blst::blst_p1, n_be: &[u8]) {
    debug_assert!(!n_be.is_empty() && n_be.len() <= 32);
    // SAFETY: as in `bls_g2::scalar_multiply`, for the G1 group.
    unsafe {
        let mut scalar = std::mem::MaybeUninit::<blst::blst_scalar>::uninit();
        blst::blst_scalar_from_be_bytes(scalar.as_mut_ptr(), n_be.as_ptr(), n_be.len());
        blst::blst_p1_mult(
            &raw mut *p,
            &raw const *p,
            scalar.as_ptr().cast::<u8>(),
            256,
        );
    }
}

/// Hash to G1 (`blst_hash_to_g1`, empty aug).
#[cfg(feature = "bls")]
fn g1_hash_to_g1(msg: &[u8], dst: &[u8]) -> blst::blst_p1 {
    // SAFETY: all pointers valid for their lengths; output fully initialized.
    unsafe {
        let mut p1 = std::mem::MaybeUninit::<blst::blst_p1>::uninit();
        blst::blst_hash_to_g1(
            p1.as_mut_ptr(),
            msg.as_ptr(),
            msg.len(),
            dst.as_ptr(),
            dst.len(),
            std::ptr::null(),
            0,
        );
        p1.assume_init()
    }
}

/// Strict G1 argument parse: a valid 48-byte atom, erroring the operator on anything else.
#[cfg(feature = "bls")]
fn g1_arg(arena: &Arena, node: NodePtr, op_name: &str) -> Result<blst::blst_p1_affine, ClvmError> {
    let blob = atom(arena, node, op_name)?;
    let bytes: [u8; 48] = blob.as_ref().try_into().map_err(|_| {
        ClvmError::InvalidInput(format!("{op_name}: atom is not G1 size, 48 bytes"))
    })?;
    bls_g1::parse_g1_compressed(&bytes).ok_or_else(|| {
        ClvmError::InvalidInput(format!(
            "{op_name}: atom is not a G1 point: {}",
            hex::encode(bytes)
        ))
    })
}

/// Strict G2 argument parse: a valid 96-byte atom, erroring the operator on anything else.
#[cfg(feature = "bls")]
fn g2_arg(arena: &Arena, node: NodePtr, op_name: &str) -> Result<blst::blst_p2, ClvmError> {
    let blob = atom(arena, node, op_name)?;
    let bytes: [u8; 96] = blob.as_ref().try_into().map_err(|_| {
        ClvmError::InvalidInput(format!("{op_name}: atom is not G2 size, 96 bytes"))
    })?;
    bls_g2::parse_g2_compressed(&bytes).ok_or_else(|| {
        ClvmError::InvalidInput(format!(
            "{op_name}: atom is not a G2 point: {}",
            hex::encode(bytes)
        ))
    })
}

/// Exactly-N proper argument list (improper tails and wrong counts error).
#[cfg(feature = "bls")]
fn get_args<const N: usize>(
    arena: &Arena,
    args: NodePtr,
    op_name: &'static str,
) -> Result<[NodePtr; N], ClvmError> {
    let mut out = [NodePtr::NIL; N];
    let mut rest = args;
    for slot in &mut out {
        let (first, r) = split(arena, rest)?;
        *slot = first;
        rest = r;
    }
    if arena.nullp(rest) {
        Ok(out)
    } else {
        Err(ClvmError::InvalidOperandArgs(op_name, N))
    }
}

/// Up-to-N argument list with actual count (silent stop on a non-pair tail, error past N).
#[cfg(feature = "bls")]
fn get_varargs<const N: usize>(
    arena: &Arena,
    args: NodePtr,
    op_name: &'static str,
) -> Result<([NodePtr; N], usize), ClvmError> {
    let mut out = [NodePtr::NIL; N];
    let mut count = 0usize;
    let mut rest = args;
    while let Some((first, r)) = arena.next(rest) {
        rest = r;
        if count == N {
            return Err(ClvmError::InvalidOperandArgs(op_name, N));
        }
        out[count] = first;
        count += 1;
    }
    Ok((out, count))
}

/// First argument minus the rest; empty input is the identity.
#[cfg(feature = "bls")]
pub fn op_bls_g1_subtract<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = BLS_G1_SUBTRACT_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut total = bls_g1::identity();
    let mut is_first = true;
    let mut input = args;
    // a non-pair tail ends the list silently
    while let Some((arg, rest)) = arena.next(input) {
        input = rest;
        let point = g1_arg(arena, arg, "g1_subtract")?;
        cost += BLS_G1_SUBTRACT_COST_PER_ARG;
        check_cost(cost, max_cost)?;
        if is_first {
            total = g1_projective(&point);
        } else {
            g1_sub_assign(&mut total, &g1_projective(&point));
        }
        is_first = false;
    }
    new_atom_and_cost(cost, &bls_g1::to_compressed(&total))
}

/// Point times a (group-order-reduced) integer scalar.
#[cfg(feature = "bls")]
pub fn op_bls_g1_multiply<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let [point, scalar] = get_args::<2>(arena, args, "g1_multiply")?;
    let mut cost = BLS_G1_MULTIPLY_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut total = g1_projective(&g1_arg(arena, point, "g1_multiply")?);
    let (scalar, scalar_len) = {
        let v = int_atom(arena, scalar, "g1_multiply")?;
        (number_from_slice(v.as_ref()), v.as_ref().len())
    };
    if (dialect.flags() & LIMITS) != 0 && scalar_len > 1024 {
        return Err(ClvmError::InvalidInput(
            "g1_multiply scalar longer than 1024 bytes".to_string(),
        ));
    }
    cost += scalar_len as u64 * BLS_G1_MULTIPLY_COST_PER_BYTE;
    check_cost(cost, max_cost)?;
    let scalar = mod_group_order(&scalar);
    g1_scalar_multiply(&mut total, scalar.to_bytes_be().1.as_slice());
    new_atom_and_cost(cost, &bls_g1::to_compressed(&total))
}

/// Flip the compressed sign bit; the point is validated unless `RELAXED_BLS` (hard fork 2)
/// is set; compressed infinity passes through unchanged.
#[cfg(feature = "bls")]
pub fn op_bls_g1_negate<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let strict = (dialect.flags() & RELAXED_BLS) == 0;
    let [point] = get_args::<1>(arena, args, "g1_negate")?;
    let mut blob: [u8; 48] = atom(arena, point, "g1_negate")?
        .as_ref()
        .try_into()
        .map_err(|_| {
            ClvmError::InvalidInput("g1_negate: atom is not a G1 size, 48 bytes".to_string())
        })?;
    if strict && bls_g1::parse_g1_compressed(&blob).is_none() {
        return Err(ClvmError::InvalidInput(format!(
            "g1_negate: atom is not a G1 point: {}",
            hex::encode(blob)
        )));
    }
    if (blob[0] & 0xe0) == 0xc0 {
        // Compressed infinity: negation is a no-op; pass the argument through, charging
        // the allocation cost anyway.
        Ok((
            BLS_G1_NEGATE_BASE_COST + 48 * MALLOC_COST_PER_BYTE,
            OpOut::Same(point),
        ))
    } else {
        blob[0] ^= 0x20;
        new_atom_and_cost(BLS_G1_NEGATE_BASE_COST, &blob)
    }
}

/// N-ary G2 sum; empty input is the identity.
#[cfg(feature = "bls")]
pub fn op_bls_g2_add<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = BLS_G2_ADD_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut total = bls_g2::identity();
    let mut input = args;
    while let Some((arg, rest)) = arena.next(input) {
        input = rest;
        let point = g2_arg(arena, arg, "g2_add")?;
        cost += BLS_G2_ADD_COST_PER_ARG;
        check_cost(cost, max_cost)?;
        bls_g2::add_assign(&mut total, &point);
    }
    new_atom_and_cost(cost, &bls_g2::to_compressed(&total))
}

/// First argument minus the rest; empty input is the identity.
#[cfg(feature = "bls")]
pub fn op_bls_g2_subtract<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = BLS_G2_SUBTRACT_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut total = bls_g2::identity();
    let mut is_first = true;
    let mut input = args;
    while let Some((arg, rest)) = arena.next(input) {
        input = rest;
        let point = g2_arg(arena, arg, "g2_subtract")?;
        cost += BLS_G2_SUBTRACT_COST_PER_ARG;
        check_cost(cost, max_cost)?;
        if is_first {
            total = point;
        } else {
            bls_g2::sub_assign(&mut total, &point);
        }
        is_first = false;
    }
    new_atom_and_cost(cost, &bls_g2::to_compressed(&total))
}

/// Point times a (group-order-reduced) integer scalar.
#[cfg(feature = "bls")]
pub fn op_bls_g2_multiply<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let [point, scalar] = get_args::<2>(arena, args, "g2_multiply")?;
    let mut cost = BLS_G2_MULTIPLY_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut total = g2_arg(arena, point, "g2_multiply")?;
    let (scalar, scalar_len) = {
        let v = int_atom(arena, scalar, "g2_multiply")?;
        (number_from_slice(v.as_ref()), v.as_ref().len())
    };
    if (dialect.flags() & LIMITS) != 0 && scalar_len > 1024 {
        return Err(ClvmError::InvalidInput(
            "g2_multiply scalar longer than 1024 bytes".to_string(),
        ));
    }
    cost += scalar_len as u64 * BLS_G2_MULTIPLY_COST_PER_BYTE;
    check_cost(cost, max_cost)?;
    let scalar = mod_group_order(&scalar);
    bls_g2::scalar_multiply(&mut total, scalar.to_bytes_be().1.as_slice());
    new_atom_and_cost(cost, &bls_g2::to_compressed(&total))
}

/// Flip the compressed sign bit; validated unless `RELAXED_BLS`; compressed infinity
/// passes through unchanged.
#[cfg(feature = "bls")]
pub fn op_bls_g2_negate<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    _max_cost: u64,
    dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let strict = (dialect.flags() & RELAXED_BLS) == 0;
    let [point] = get_args::<1>(arena, args, "g2_negate")?;
    let mut blob: [u8; 96] = atom(arena, point, "g2_negate")?
        .as_ref()
        .try_into()
        .map_err(|_| {
            ClvmError::InvalidInput("g2_negate: atom is not G2 size, 96 bytes".to_string())
        })?;
    if strict && bls_g2::parse_g2_compressed(&blob).is_none() {
        return Err(ClvmError::InvalidInput(format!(
            "g2_negate: atom is not a G2 point: {}",
            hex::encode(blob)
        )));
    }
    if (blob[0] & 0xe0) == 0xc0 {
        Ok((
            BLS_G2_NEGATE_BASE_COST + 96 * MALLOC_COST_PER_BYTE,
            OpOut::Same(point),
        ))
    } else {
        blob[0] ^= 0x20;
        new_atom_and_cost(BLS_G2_NEGATE_BASE_COST, &blob)
    }
}

/// Hash a message to G1 with an optional explicit DST (default: the G1 AUG-scheme DST).
#[cfg(feature = "bls")]
pub fn op_bls_map_to_g1<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let ([msg, dst], argc) = get_varargs::<2>(arena, args, "g1_map")?;
    if !(1..=2).contains(&argc) {
        return Err(ClvmError::InvalidArgCount(format!(
            "g1_map takes exactly 1 or 2 arguments, got {argc}"
        )));
    }
    let mut cost = BLS_MAP_TO_G1_BASE_COST;
    check_cost(cost, max_cost)?;
    let msg = atom(arena, msg, "g1_map")?.as_ref().to_vec();
    cost += msg.len() as u64 * BLS_MAP_TO_G1_COST_PER_BYTE;
    check_cost(cost, max_cost)?;
    let dst = if argc == 2 {
        atom(arena, dst, "g1_map")?.as_ref().to_vec()
    } else {
        DST_G1.to_vec()
    };
    cost += dst.len() as u64 * BLS_MAP_TO_G1_COST_PER_DST_BYTE;
    check_cost(cost, max_cost)?;
    let point = g1_hash_to_g1(&msg, &dst);
    new_atom_and_cost(cost, &bls_g1::to_compressed(&point))
}

/// Hash a message to G2 with an optional explicit DST (default: the G2 AUG-scheme DST).
/// No interim cost check between the message and DST byte charges.
#[cfg(feature = "bls")]
pub fn op_bls_map_to_g2<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let ([msg, dst], argc) = get_varargs::<2>(arena, args, "g2_map")?;
    if !(1..=2).contains(&argc) {
        return Err(ClvmError::InvalidArgCount(format!(
            "g2_map takes exactly 1 or 2 arguments, got {argc}"
        )));
    }
    let mut cost = BLS_MAP_TO_G2_BASE_COST;
    check_cost(cost, max_cost)?;
    let msg = atom(arena, msg, "g2_map")?.as_ref().to_vec();
    cost += msg.len() as u64 * BLS_MAP_TO_G2_COST_PER_BYTE;
    let dst = if argc == 2 {
        atom(arena, dst, "g2_map")?.as_ref().to_vec()
    } else {
        DST_G2.to_vec()
    };
    cost += dst.len() as u64 * BLS_MAP_TO_G2_COST_PER_DST_BYTE;
    check_cost(cost, max_cost)?;
    let point = bls_g2::hash_to_g2(&msg, &dst);
    new_atom_and_cost(cost, &bls_g2::to_compressed(&point))
}

/// A flat list of `(G1, G2)` pairs; returns nil iff the aggregated pairing is the
/// identity, otherwise the program TERMINATES with an error.
#[cfg(feature = "bls")]
pub fn op_bls_pairing_identity<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = BLS_PAIRING_BASE_COST;
    check_cost(cost, max_cost)?;
    let mut items = Vec::<(blst::blst_p1, blst::blst_p2)>::new();
    let mut rest = args;
    while !arena.nullp(rest) {
        cost += BLS_PAIRING_COST_PER_ARG;
        check_cost(cost, max_cost)?;
        let (g1_node, r) = split(arena, rest)?;
        let g1 = g1_projective(&g1_arg(arena, g1_node, "bls_pairing_identity")?);
        let (g2_node, r) = split(arena, r)?;
        let g2 = g2_arg(arena, g2_node, "bls_pairing_identity")?;
        rest = r;
        items.push((g1, g2));
    }
    if bls_g2::aggregate_pairing(&items) {
        Ok((cost, OpOut::Same(NodePtr::NIL)))
    } else {
        Err(ClvmError::InvalidInput(
            "bls_pairing_identity failed".to_string(),
        ))
    }
}

/// `(sig pk1 msg1 pk2 msg2 ...)` — AUG-scheme aggregate verify; returns nil on success,
/// otherwise the program TERMINATES with an error.
#[cfg(feature = "bls")]
pub fn op_bls_verify<D: Dialect>(
    arena: &Arena,
    args: NodePtr,
    max_cost: u64,
    _dialect: &D,
) -> Result<(u64, OpOut), ClvmError> {
    let mut cost = BLS_PAIRING_BASE_COST;
    check_cost(cost, max_cost)?;
    let (sig_node, mut rest) = split(arena, args)?;
    let signature = g2_arg(arena, sig_node, "bls_verify")?;
    let mut items = Vec::<(blst::blst_p1, Vec<u8>)>::new();
    while !arena.nullp(rest) {
        let (pk_node, r) = split(arena, rest)?;
        let pk = g1_projective(&g1_arg(arena, pk_node, "bls_verify")?);
        let (msg_node, r) = split(arena, r)?;
        let msg = atom(arena, msg_node, "bls_verify message")?
            .as_ref()
            .to_vec();
        rest = r;
        cost += BLS_PAIRING_COST_PER_ARG;
        cost += msg.len() as u64 * BLS_MAP_TO_G2_COST_PER_BYTE;
        cost += DST_G2.len() as u64 * BLS_MAP_TO_G2_COST_PER_DST_BYTE;
        check_cost(cost, max_cost)?;
        items.push((pk, msg));
    }
    if bls_g2::aggregate_verify(&signature, &items) {
        Ok((cost, OpOut::Same(NodePtr::NIL)))
    } else {
        Err(ClvmError::InvalidInput("bls_verify failed".to_string()))
    }
}

// ---- bls-feature-less stubs: the VM stays available, the operators error.
// ---- A `bls`-less build is NOT consensus-capable.

#[cfg(not(feature = "bls"))]
macro_rules! bls_stub {
    ($name:ident) => {
        pub fn $name<D: Dialect>(
            _arena: &Arena,
            _args: NodePtr,
            _max_cost: u64,
            _dialect: &D,
        ) -> Result<(u64, OpOut), ClvmError> {
            Err(ClvmError::Unsupported(format!(
                "{} requires dg_xch_core to be built with the `bls` feature",
                stringify!($name)
            )))
        }
    };
}

#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g1_subtract);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g1_multiply);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g1_negate);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g2_add);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g2_subtract);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g2_multiply);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_g2_negate);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_map_to_g1);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_map_to_g2);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_pairing_identity);
#[cfg(not(feature = "bls"))]
bls_stub!(op_bls_verify);
