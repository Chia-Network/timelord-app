//! Validated bounded schedule. Ordering and length are owned by consensus.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::io::{Error, ErrorKind};
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FilterSchedule {
    heights: [u32; 9],
    len: u8,
}
impl FilterSchedule {
    pub const fn fixed(heights: [u32; 9]) -> Self {
        let mut i = 0;
        while i < 9 {
            assert!(heights[i] > 0);
            if i > 0 {
                assert!(heights[i - 1] > heights[i]);
            }
            i += 1;
        }
        Self { heights, len: 9 }
    }
    pub fn try_from_slice(values: &[u32]) -> Result<Self, Error> {
        if values.len() > 9 || values.contains(&0) || values.windows(2).any(|p| p[0] <= p[1]) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "filter heights must be at most nine positive descending values",
            ));
        }
        let mut heights = [0; 9];
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
        if relative == 0 {
            return initial;
        }
        self.entries()
            .iter()
            .enumerate()
            .find(|(_, h)| relative >= **h)
            .map_or(initial, |(i, _)| initial.saturating_sub(self.len - i as u8))
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
