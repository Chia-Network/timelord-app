use crate::clvm::sexp::{AtomBuf, SExp};
use crate::errors::ClvmError;
use crate::formatting::number_from_slice;
use num_bigint::{BigInt, Sign};
use num_traits::{FromPrimitive, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::fmt::Display;
use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Rem, Sub, SubAssign};

#[derive(Clone)]
pub enum SExpNumber {
    I128(i128),
    BigInt(BigInt),
}
impl Display for SExpNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SExpNumber::I128(i) => write!(f, "{}", i),
            SExpNumber::BigInt(i) => write!(f, "{}", i),
        }
    }
}
impl SExpNumber {
    pub fn sign(&self) -> Sign {
        match self {
            SExpNumber::I128(i) => {
                if *i == 0 {
                    Sign::NoSign
                } else if *i > 0 {
                    Sign::Plus
                } else {
                    Sign::Minus
                }
            }
            SExpNumber::BigInt(int) => int.sign(),
        }
    }
    pub fn to_u64(&self) -> Option<u64> {
        match self {
            SExpNumber::I128(i) => u64::from_i128(*i),
            SExpNumber::BigInt(b) => b.to_u64(),
        }
    }
    #[inline]
    pub fn div_mod_floor(&self, rhs: &SExpNumber) -> (SExpNumber, SExpNumber) {
        let (d, r) = self.div_rem(rhs);
        if (r.sign() == Sign::Plus && rhs.sign() == Sign::Minus)
            || (r.sign() == Sign::Minus && rhs.sign() == Sign::Plus)
        {
            (d - SExpNumber::I128(1), r + rhs)
        } else {
            (d, r)
        }
    }
    #[inline]
    fn div_rem(&self, rhs: &Self) -> (Self, Self) {
        (self / rhs, self % rhs)
    }
}
impl<'a> From<&AtomBuf<'a>> for SExpNumber {
    fn from(buf: &AtomBuf) -> Self {
        // Small atoms are sign-extended into i128; larger atoms use BigInt.
        let buf = buf.as_ref();
        match buf.len() {
            0 => Self::I128(0),
            x if x <= 16 => {
                let fill = if buf[0] & 0x80 != 0 { 0xff } else { 0x00 };
                let mut int_buf = [fill; 16];
                int_buf[(16 - x)..].copy_from_slice(buf);
                Self::I128(i128::from_be_bytes(int_buf))
            }
            _ => Self::BigInt(number_from_slice(buf)),
        }
    }
}
pub type SExpNumberWithLen = (SExpNumber, usize);
impl<'a> TryFrom<&SExp<'a>> for SExpNumberWithLen {
    type Error = ClvmError;
    fn try_from(sexp: &SExp<'a>) -> Result<SExpNumberWithLen, ClvmError> {
        let atom = sexp.atom()?;
        Ok((SExpNumber::from(atom), atom.as_ref().len()))
    }
}
impl From<SExpNumber> for SExp<'static> {
    fn from(sexp: SExpNumber) -> SExp<'static> {
        match sexp {
            SExpNumber::I128(num) => SExp::from(num),
            SExpNumber::BigInt(num) => SExp::from(&num),
        }
    }
}
impl Zero for SExpNumber {
    fn zero() -> Self {
        SExpNumber::I128(0)
    }
    #[inline]
    fn is_zero(&self) -> bool {
        match self {
            SExpNumber::I128(i) => i.is_zero(),
            SExpNumber::BigInt(b) => b.is_zero(),
        }
    }
}
impl Add<&SExpNumber> for SExpNumber {
    type Output = SExpNumber;
    fn add(self, rhs: &SExpNumber) -> SExpNumber {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_add(*r) {
                Some(v) => SExpNumber::I128(v),
                None => SExpNumber::BigInt(BigInt::from(l) + r),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(l + r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => {
                SExpNumber::BigInt(l + BigInt::from(*r))
            }
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(BigInt::from(l) + r),
        }
    }
}
impl Add<SExpNumber> for SExpNumber {
    type Output = SExpNumber;
    fn add(self, rhs: SExpNumber) -> SExpNumber {
        self + &rhs
    }
}

impl AddAssign for SExpNumber {
    fn add_assign(&mut self, rhs: SExpNumber) {
        match (&mut *self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_add(r) {
                Some(v) => *self = SExpNumber::I128(v),
                None => *self = SExpNumber::BigInt(BigInt::from(*l) + BigInt::from(r)),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => *l += r,
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => *l += BigInt::from(r),
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => {
                *self = SExpNumber::BigInt(BigInt::from(*l) + r)
            }
        }
    }
}
impl Sub<&SExpNumber> for SExpNumber {
    type Output = SExpNumber;
    fn sub(self, rhs: &SExpNumber) -> SExpNumber {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_sub(*r) {
                Some(v) => SExpNumber::I128(v),
                None => SExpNumber::BigInt(BigInt::from(l) - r),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(l - r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => {
                SExpNumber::BigInt(l - BigInt::from(*r))
            }
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(BigInt::from(l) - r),
        }
    }
}
impl Sub<SExpNumber> for SExpNumber {
    type Output = SExpNumber;
    fn sub(self, rhs: SExpNumber) -> SExpNumber {
        self - &rhs
    }
}
impl SubAssign for SExpNumber {
    fn sub_assign(&mut self, rhs: SExpNumber) {
        match (&mut *self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_sub(r) {
                Some(v) => *self = SExpNumber::I128(v),
                None => *self = SExpNumber::BigInt(BigInt::from(*l) - BigInt::from(r)),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => *l -= r,
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => *l -= BigInt::from(r),
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => {
                *self = SExpNumber::BigInt(BigInt::from(*l) - r)
            }
        }
    }
}
impl Mul<&SExpNumber> for SExpNumber {
    type Output = SExpNumber;
    fn mul(self, rhs: &SExpNumber) -> SExpNumber {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_mul(*r) {
                Some(v) => SExpNumber::I128(v),
                None => SExpNumber::BigInt(BigInt::from(l) * r),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(l * r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => {
                SExpNumber::BigInt(l * BigInt::from(*r))
            }
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(BigInt::from(l) * r),
        }
    }
}
impl Mul for SExpNumber {
    type Output = SExpNumber;
    fn mul(self, rhs: SExpNumber) -> SExpNumber {
        self * &rhs
    }
}
impl MulAssign for SExpNumber {
    fn mul_assign(&mut self, rhs: SExpNumber) {
        match (&mut *self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_mul(r) {
                Some(v) => *self = SExpNumber::I128(v),
                None => *self = SExpNumber::BigInt(BigInt::from(*l) * BigInt::from(r)),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => *l *= r,
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => *l *= BigInt::from(r),
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => {
                *self = SExpNumber::BigInt(BigInt::from(*l) * r)
            }
        }
    }
}
impl Div for &SExpNumber {
    type Output = SExpNumber;
    fn div(self, rhs: &SExpNumber) -> SExpNumber {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_div(*r) {
                Some(v) => SExpNumber::I128(v),
                None => SExpNumber::BigInt(BigInt::from(*l) / r),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(l / r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => {
                SExpNumber::BigInt(l / BigInt::from(*r))
            }
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => {
                SExpNumber::BigInt(BigInt::from(*l) / r)
            }
        }
    }
}
impl Div for SExpNumber {
    type Output = SExpNumber;
    fn div(self, rhs: SExpNumber) -> SExpNumber {
        &self / &rhs
    }
}
impl Rem for &SExpNumber {
    type Output = SExpNumber;
    fn rem(self, rhs: &SExpNumber) -> SExpNumber {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => match l.checked_rem(*r) {
                Some(v) => SExpNumber::I128(v),
                None => SExpNumber::BigInt(BigInt::from(*l) % r),
            },
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => SExpNumber::BigInt(l % r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => {
                SExpNumber::BigInt(l % BigInt::from(*r))
            }
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => {
                SExpNumber::BigInt(BigInt::from(*l) % r)
            }
        }
    }
}
impl Rem for SExpNumber {
    type Output = SExpNumber;
    fn rem(self, rhs: SExpNumber) -> SExpNumber {
        &self % &rhs
    }
}

impl PartialEq for SExpNumber {
    fn eq(&self, rhs: &Self) -> bool {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => l == r,
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => l == r,
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => *l == BigInt::from(*r),
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => BigInt::from(*l) == *r,
        }
    }
}
impl Eq for SExpNumber {}
impl PartialOrd for SExpNumber {
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}

impl Ord for SExpNumber {
    fn cmp(&self, rhs: &Self) -> Ordering {
        match (self, rhs) {
            (SExpNumber::I128(l), SExpNumber::I128(r)) => l.cmp(r),
            (SExpNumber::BigInt(l), SExpNumber::BigInt(r)) => l.cmp(r),
            (SExpNumber::BigInt(l), SExpNumber::I128(r)) => l.cmp(&BigInt::from(*r)),
            (SExpNumber::I128(l), SExpNumber::BigInt(r)) => BigInt::from(*l).cmp(r),
        }
    }
}
