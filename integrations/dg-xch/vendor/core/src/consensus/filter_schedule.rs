//! Validated bounded schedule. Ordering and length are owned by consensus.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::io::{Error, ErrorKind};
/// Maximum number of relative-height entries a filter schedule can hold.
pub const MAX: usize = 9;
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FilterSchedule {
    heights: [u32; MAX],
    len: u8,
}
impl FilterSchedule {
    pub const fn fixed(heights: [u32; MAX]) -> Self {
        let mut i = 0;
        while i < MAX {
            assert!(heights[i] > 0);
            if i > 0 {
                assert!(heights[i - 1] > heights[i]);
            }
            i += 1;
        }
        Self {
            heights,
            len: MAX as u8,
        }
    }
    pub fn try_from_slice(values: &[u32]) -> Result<Self, Error> {
        if values.len() > MAX || values.contains(&0) || values.windows(2).any(|p| p[0] <= p[1]) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("filter heights must be at most {MAX} positive descending values"),
            ));
        }
        let mut heights = [0; MAX];
        heights[..values.len()].copy_from_slice(values);
        Ok(Self {
            heights,
            len: values.len() as u8,
        })
    }
    pub fn entries(&self) -> &[u32] {
        &self.heights[..usize::from(self.len)]
    }
    pub fn base_bits(&self, height: u32, fork: u32, initial: u8) -> u8 {
        let relative = height.saturating_sub(fork);
        self.entries()
            .iter()
            .enumerate()
            .find(|(_, h)| relative >= **h)
            .map_or(initial, |(i, _)| {
                initial.saturating_sub(u8::try_from(usize::from(self.len) - i).unwrap_or(u8::MAX))
            })
    }
}
impl Serialize for FilterSchedule {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.entries().serialize(s)
    }
}
impl<'de> Deserialize<'de> for FilterSchedule {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Vec::<u32>::deserialize(d)?;
        Self::try_from_slice(&v).map_err(serde::de::Error::custom)
    }
}
/// Frozen defaults used for omission in legacy identity serialization, independent of MAINNET.
pub const LEGACY_SCHEDULE: FilterSchedule = FilterSchedule::fixed([
    50494000, 45444000, 40394000, 35343000, 30298000, 25247000, 20197000, 15146000, 10101000,
]);
/// Frozen legacy filter window. It must not follow the evolving MAINNET constants.
pub const LEGACY_FILTER_WINDOW_SIZE: u8 = 16;
/// Frozen legacy effective-filter cap. It must not follow the evolving MAINNET constants.
pub const LEGACY_MAX_EFFECTIVE_PLOT_FILTER_BITS: u8 = 13;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_roundtrip() {
        for json in ["[0]", "[10,20]", "[10,10]", "[10,9,8,7,6,5,4,3,2,1]"] {
            assert!(serde_json::from_str::<FilterSchedule>(json).is_err());
        }
        for values in [&[][..], &[10, 5][..]] {
            let s = FilterSchedule::try_from_slice(values).unwrap();
            assert_eq!(
                serde_json::from_str::<FilterSchedule>(&serde_json::to_string(&s).unwrap())
                    .unwrap(),
                s
            );
        }
    }
}
