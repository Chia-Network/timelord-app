//! Chia packs presence of two optional trailing fields into bits 0 and 1.
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::{Cursor, Error, ErrorKind};
pub(super) fn append<A: ChiaSerialize, B: ChiaSerialize>(
    a: Option<&A>,
    b: Option<&B>,
    out: &mut Vec<u8>,
    version: ChiaProtocolVersion,
) -> Result<(), Error> {
    (u8::from(a.is_some()) | (u8::from(b.is_some()) << 1)).append_bytes(out, version)?;
    if let Some(v) = a {
        v.append_bytes(out, version)?;
    }
    if let Some(v) = b {
        v.append_bytes(out, version)?;
    }
    Ok(())
}
pub(super) fn parse<A: ChiaSerialize, B: ChiaSerialize>(
    bytes: &mut Cursor<&[u8]>,
    version: ChiaProtocolVersion,
) -> Result<(Option<A>, Option<B>), Error> {
    let flags = u8::from_bytes(bytes, version)?;
    if flags > 3 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "invalid packed optional flags",
        ));
    }
    let a = if flags & 1 != 0 {
        Some(A::from_bytes(bytes, version)?)
    } else {
        None
    };
    let b = if flags & 2 != 0 {
        Some(B::from_bytes(bytes, version)?)
    } else {
        None
    };
    Ok((a, b))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_presence_flags_and_invalid_bits() {
        let version = ChiaProtocolVersion::default();
        for a in [None, Some(17u64)] {
            for b in [None, Some(42u32)] {
                let mut bytes = Vec::new();
                append(a.as_ref(), b.as_ref(), &mut bytes, version).unwrap();
                let mut cursor = Cursor::new(bytes.as_slice());
                assert_eq!(parse(&mut cursor, version).unwrap(), (a, b));
                assert_eq!(cursor.position(), bytes.len() as u64);
            }
        }
        assert!(parse::<u64, u32>(&mut Cursor::new(&[4]), version).is_err());
    }
}
