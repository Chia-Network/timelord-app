use crate::blockchain::sized_bytes::Bytes32;
use crate::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Error, ErrorKind};

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct SubEpochSummary {
    pub prev_subepoch_summary_hash: Bytes32,
    pub reward_chain_hash: Bytes32,
    pub num_blocks_overflow: u8,
    pub new_difficulty: Option<u64>,
    pub new_sub_slot_iters: Option<u64>,
    #[serde(default)]
    pub challenge_merkle_root: Option<Bytes32>,
}
impl SubEpochSummary {
    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
impl ChiaSerialize for SubEpochSummary {
    fn to_bytes(&self, version: ChiaProtocolVersion) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        self.append_bytes(&mut bytes, version)?;
        Ok(bytes)
    }
    fn append_bytes(&self, bytes: &mut Vec<u8>, version: ChiaProtocolVersion) -> Result<(), Error> {
        self.prev_subepoch_summary_hash
            .append_bytes(bytes, version)?;
        self.reward_chain_hash.append_bytes(bytes, version)?;
        self.num_blocks_overflow.append_bytes(bytes, version)?;
        self.new_difficulty.append_bytes(bytes, version)?;
        let flags = u8::from(self.new_sub_slot_iters.is_some())
            | (u8::from(self.challenge_merkle_root.is_some()) << 1);
        flags.append_bytes(bytes, version)?;
        if let Some(v) = self.new_sub_slot_iters {
            v.append_bytes(bytes, version)?;
        }
        if let Some(v) = self.challenge_merkle_root {
            v.append_bytes(bytes, version)?;
        }
        Ok(())
    }
    fn from_bytes(bytes: &mut Cursor<&[u8]>, version: ChiaProtocolVersion) -> Result<Self, Error> {
        let prev_subepoch_summary_hash = ChiaSerialize::from_bytes(bytes, version)?;
        let reward_chain_hash = ChiaSerialize::from_bytes(bytes, version)?;
        let num_blocks_overflow = ChiaSerialize::from_bytes(bytes, version)?;
        let new_difficulty = ChiaSerialize::from_bytes(bytes, version)?;
        let flags = u8::from_bytes(bytes, version)?;
        if flags > 3 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid sub-epoch optional flags",
            ));
        }
        let new_sub_slot_iters = if flags & 1 != 0 {
            Some(ChiaSerialize::from_bytes(bytes, version)?)
        } else {
            None
        };
        let challenge_merkle_root = if flags & 2 != 0 {
            Some(ChiaSerialize::from_bytes(bytes, version)?)
        } else {
            None
        };
        Ok(Self {
            prev_subepoch_summary_hash,
            reward_chain_hash,
            num_blocks_overflow,
            new_difficulty,
            new_sub_slot_iters,
            challenge_merkle_root,
        })
    }
}
impl From<&SubEpochSummary> for crate::clvm::sexp::SExp<'static> {
    fn from(val: &SubEpochSummary) -> Self {
        (&[
            crate::clvm::sexp::SExp::from(&val.prev_subepoch_summary_hash),
            crate::clvm::sexp::SExp::from(&val.reward_chain_hash),
            crate::clvm::sexp::SExp::from(&val.num_blocks_overflow),
            crate::clvm::sexp::SExp::from(&val.new_difficulty),
            crate::clvm::sexp::SExp::from(&val.new_sub_slot_iters),
            crate::clvm::sexp::SExp::from(&val.challenge_merkle_root),
        ])
            .into()
    }
}
impl From<SubEpochSummary> for crate::clvm::sexp::SExp<'static> {
    fn from(val: SubEpochSummary) -> Self {
        Self::from(&val)
    }
}
