use crate::errors::ClvmError;
use hex::{FromHexError, decode};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero, pow};
use once_cell::sync::Lazy;
use serde::Deserializer;
use serde::de::Visitor;
use std::fmt::Formatter;
use std::io::{Error, ErrorKind};
use std::marker::PhantomData;
use std::str::FromStr;

pub static BIG_ZERO: Lazy<BigInt> = Lazy::new(|| BigInt::from(0));
pub static BIG_ONE: Lazy<BigInt> = Lazy::new(|| BigInt::from(1));
pub static BIG_TWO: Lazy<BigInt> = Lazy::new(|| BigInt::from(2));

#[must_use]
pub fn prep_hex_str<S: AsRef<str>>(to_fix: S) -> String {
    let lc = to_fix.as_ref().to_lowercase();
    if let Some(s) = lc.strip_prefix("0x") {
        s.to_string()
    } else {
        lc
    }
}

pub fn hex_to_bytes<S: AsRef<str>>(hex: S) -> Result<Vec<u8>, FromHexError> {
    decode(prep_hex_str(hex))
}

#[must_use]
pub fn number_from_slice(v: &[u8]) -> BigInt {
    if v.is_empty() {
        BigInt::zero()
    } else {
        BigInt::from_signed_bytes_be(v)
    }
}

#[must_use]
pub fn u64_to_bytes(v: u64) -> Vec<u8> {
    let mut rtn = Vec::new();
    if v.leading_zeros() == 0 {
        rtn.push(u8::MIN);
        let ary = v.to_be_bytes();
        rtn.extend_from_slice(&ary);
        rtn
    } else {
        let mut trim: bool = true;
        for b in v.to_be_bytes() {
            if trim {
                if b == u8::MIN {
                    continue;
                }
                rtn.push(b);
                trim = false;
            } else {
                rtn.push(b);
            }
        }
        rtn
    }
}

#[allow(clippy::cast_possible_truncation)]
pub fn bigint_to_bytes(v: &BigInt, signed: bool) -> Vec<u8> {
    // Minimal big-endian two's-complement. num-bigint's to_signed_bytes_be is the
    // canonical encoding; strip redundant sign bytes to minimal form.
    if v.is_zero() {
        return vec![];
    }
    if !signed {
        return v.magnitude().to_bytes_be();
    }
    let mut b = v.to_signed_bytes_be();
    while b.len() > 1 && ((b[0] == 0x00 && b[1] & 0x80 == 0) || (b[0] == 0xFF && b[1] & 0x80 != 0))
    {
        b.remove(0);
    }
    b
}

#[allow(clippy::cast_possible_truncation)]
pub fn signed_bigint_to_bytes(v_: &BigInt, signed: bool) -> Result<Vec<u8>, ClvmError> {
    let v = v_.clone();
    if v == *BIG_ZERO {
        return Ok(vec![]);
    }
    if !signed && v.is_negative() {
        return Err(ClvmError::Overflow(
            "OverflowError: can't convert negative int to unsigned".to_string(),
        ));
    }
    let mut byte_count = 1;
    let mut dec = 0;
    let div = if signed {
        BIG_ONE.clone()
    } else {
        BIG_ZERO.clone()
    };
    let b_pow = BigInt::from(1_u64 << 32); // b32.to_bigint().unwrap();
    if v.is_negative() {
        let mut right_hand = (-(v.clone()) + BIG_ONE.clone()) * (div + BIG_ONE.clone());
        while pow(b_pow.clone(), (byte_count - 1) / 4 + 1) < right_hand {
            byte_count += 4;
        }
        right_hand = -(v.clone()) * BIG_TWO.clone();
        while pow(BIG_TWO.clone(), 8 * byte_count) < right_hand {
            byte_count += 1;
        }
    } else {
        let mut right_hand = (v.clone() + BIG_ONE.clone()) * (div.clone() + BIG_ONE.clone());
        while pow(b_pow.clone(), (byte_count - 1) / 4 + 1) < right_hand {
            byte_count += 4;
        }
        right_hand = (v.clone() + BIG_ONE.clone()) * (div + BIG_ONE.clone());
        while pow(BigInt::from(2_u32), 8 * byte_count) < right_hand {
            byte_count += 1;
        }
    }
    let extra_byte = usize::from(
        signed
            && v > *BIG_ZERO
            && ((v.clone() >> ((byte_count - 1) * 8)) & BigInt::from(0x80_u32)) > *BIG_ZERO,
    );
    let total_bytes = byte_count + extra_byte;
    let mut dv = Vec::<u8>::with_capacity(total_bytes);
    let byte4_remain = byte_count % 4;
    let byte4_length = (byte_count - byte4_remain) / 4;
    dv.resize(total_bytes, 0);
    let (_sign, u32_digits) = v.to_u32_digits();
    for (i, n) in u32_digits.iter().take(byte4_length).enumerate() {
        let word_idx = byte4_length - i - 1;
        let num = u64::from(*n);
        let pointer = extra_byte + byte4_remain + word_idx * 4;
        let setval = if v.is_negative() {
            (1_u64 << 32) - num - u64::from(dec)
        } else {
            num
        };
        dec = 1;
        dv[pointer] = ((setval >> 24) & 0xff) as u8;
        dv[pointer + 1] = ((setval >> 16) & 0xff) as u8;
        dv[pointer + 2] = ((setval >> 8) & 0xff) as u8;
        dv[pointer + 3] = (setval & 0xff) as u8;
    }

    let last_bytes = u32_digits[u32_digits.len() - 1];
    let transform = |idx| {
        if v.is_negative() {
            (((1 << (8 * byte4_remain)) - last_bytes - dec) >> (8 * idx)) as u8
        } else {
            (last_bytes >> (8 * idx)) as u8
        }
    };

    for i in 0..byte4_remain {
        dv[extra_byte + i] = transform(byte4_remain - i - 1);
    }

    Ok(dv)
}

pub fn u64_from_bigint(item: &BigInt) -> Result<u64, Error> {
    if item.is_negative() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "cannot convert negative integer to u64",
        ));
    }
    item.to_u64()
        .ok_or(Error::new(ErrorKind::InvalidData, "u64::MAX overflow"))
}

/// Saturating decode of a signed CLVM time-lock argument into the `u32` slot the
/// `ASSERT_HEIGHT_*` / `ASSERT_BEFORE_HEIGHT_*` bounds are stored in.
///
/// CLVM integers are signed two's-complement, but the height fields are `u32`.
/// A negative bound clamps to `0` and an out-of-range positive bound (`> u32::MAX`)
/// clamps to `u32::MAX`. Which of those is a no-op and which is a hard failure
/// then follows from the assertion's direction:
/// - `ASSERT_HEIGHT_*` (satisfied when `block_height >= bound`): a negative bound
///   is trivially satisfied (a constraint about the past), an overflow bound is
///   unreachable and fails.
/// - `ASSERT_BEFORE_HEIGHT_*` (satisfied when `block_height < bound`): a negative
///   bound is unsatisfiable and fails, an overflow bound is trivially satisfied.
#[must_use]
pub fn saturating_u32_from_bigint(item: &BigInt) -> u32 {
    if item.is_negative() {
        0
    } else {
        item.to_u32().unwrap_or(u32::MAX)
    }
}

/// Saturating decode of a signed CLVM time-lock argument into the `u64` slot the
/// `ASSERT_SECONDS_*` / `ASSERT_BEFORE_SECONDS_*` bounds are stored in.
///
/// The seconds analogue of [`saturating_u32_from_bigint`]: a negative bound clamps
/// to `0`, an out-of-range positive bound (`> u64::MAX`) clamps to `u64::MAX`, and
/// the assertion direction decides which end is a no-op and which is a failure.
#[must_use]
pub fn saturating_u64_from_bigint(item: &BigInt) -> u64 {
    if item.is_negative() {
        0
    } else {
        item.to_u64().unwrap_or(u64::MAX)
    }
}

fn u32_from_slice_impl(buf: &[u8], signed: bool) -> Option<u32> {
    if buf.is_empty() {
        return Some(0);
    }
    // too many bytes for u32
    if buf.len() > 4 {
        return None;
    }
    let sign_extend = (buf[0] & 0x80) != 0;
    let mut ret: u32 = if signed && sign_extend {
        0xffff_ffff
    } else {
        0
    };
    for b in buf {
        ret <<= 8;
        ret |= u32::from(*b);
    }
    Some(ret)
}

#[must_use]
pub fn u32_from_slice(buf: &[u8]) -> Option<u32> {
    u32_from_slice_impl(buf, false)
}

#[allow(clippy::cast_possible_wrap)]
#[must_use]
pub fn i32_from_slice(buf: &[u8]) -> Option<i32> {
    u32_from_slice_impl(buf, true).map(|v| v as i32)
}

pub fn parse_u128<'de, D>(d: D) -> Result<u128, D::Error>
where
    D: Deserializer<'de>,
{
    struct PU128(PhantomData<fn() -> u128>);
    impl Visitor<'_> for PU128 {
        type Value = u128;

        fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
            formatter.write_str("string or number")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_u8(u8::from(v))
                .ok_or_else(|| serde::de::Error::custom("Invalid Value for bool"))
        }

        fn visit_i8<E>(self, v: i8) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_i8(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for i8"))
        }

        fn visit_i16<E>(self, v: i16) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_i16(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for i16"))
        }

        fn visit_i32<E>(self, v: i32) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_i32(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for i32"))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_i64(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for i64"))
        }

        fn visit_u8<E>(self, v: u8) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_u8(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for u8"))
        }

        fn visit_u16<E>(self, v: u16) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_u16(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for u16"))
        }

        fn visit_u32<E>(self, v: u32) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_u32(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for u32"))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_u64(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for u64"))
        }

        fn visit_f32<E>(self, v: f32) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_f32(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for f32"))
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_f64(v).ok_or_else(|| serde::de::Error::custom("Invalid Value for f64"))
        }

        fn visit_char<E>(self, v: char) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_str(&v.to_string())
                .map_err(|_| serde::de::Error::custom("Invalid Value for char"))
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_str(v).map_err(|_| serde::de::Error::custom("Invalid Value for str"))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            u128::from_str(&v).map_err(|_| serde::de::Error::custom("Invalid Value for string"))
        }
    }
    d.deserialize_any(PU128(PhantomData))
}
