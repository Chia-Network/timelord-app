use super::proof_encoding::ProofEncoding;
use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RewardChainBlockUnfinished {
    pub total_iters: u128,
    pub signage_point_index: u8,
    pub pos_ss_cc_challenge_hash: Bytes32,
    pub proof_of_space: ProofOfSpace,
    pub challenge_chain_sp_vdf: Option<VdfInfo>,
    pub challenge_chain_sp_signature: Bytes96,
    pub reward_chain_sp_vdf: Option<VdfInfo>,
    pub reward_chain_sp_signature: Bytes96,
}

impl RewardChainBlockUnfinished {
    fn append_fields(
        &self,
        bytes: &mut Vec<u8>,
        version: ChiaProtocolVersion,
        encoding: ProofEncoding,
    ) -> Result<(), Error> {
        self.total_iters.append_bytes(bytes, version)?;
        self.signage_point_index.append_bytes(bytes, version)?;
        self.pos_ss_cc_challenge_hash.append_bytes(bytes, version)?;
        encoding.append(&self.proof_of_space, bytes, version)?;
        self.challenge_chain_sp_vdf.append_bytes(bytes, version)?;
        self.challenge_chain_sp_signature
            .append_bytes(bytes, version)?;
        self.reward_chain_sp_vdf.append_bytes(bytes, version)?;
        self.reward_chain_sp_signature
            .append_bytes(bytes, version)?;
        Ok(())
    }
    pub fn hash(&self) -> Result<Bytes32, Error> {
        let mut bytes = Vec::new();
        self.append_fields(
            &mut bytes,
            ChiaProtocolVersion::default(),
            ProofEncoding::Commitment,
        )?;
        Ok(Bytes32::from(hash_256(bytes)))
    }
}

impl ChiaSerialize for RewardChainBlockUnfinished {
    fn to_bytes(&self, version: ChiaProtocolVersion) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        self.append_bytes(&mut bytes, version)?;
        Ok(bytes)
    }
    fn append_bytes(&self, bytes: &mut Vec<u8>, version: ChiaProtocolVersion) -> Result<(), Error> {
        self.append_fields(bytes, version, ProofEncoding::Wire)
    }
    fn from_bytes(
        bytes: &mut std::io::Cursor<&[u8]>,
        version: ChiaProtocolVersion,
    ) -> Result<Self, Error> {
        Ok(Self {
            total_iters: ChiaSerialize::from_bytes(bytes, version)?,
            signage_point_index: ChiaSerialize::from_bytes(bytes, version)?,
            pos_ss_cc_challenge_hash: ChiaSerialize::from_bytes(bytes, version)?,
            proof_of_space: ChiaSerialize::from_bytes(bytes, version)?,
            challenge_chain_sp_vdf: ChiaSerialize::from_bytes(bytes, version)?,
            challenge_chain_sp_signature: ChiaSerialize::from_bytes(bytes, version)?,
            reward_chain_sp_vdf: ChiaSerialize::from_bytes(bytes, version)?,
            reward_chain_sp_signature: ChiaSerialize::from_bytes(bytes, version)?,
        })
    }
}
impl From<&RewardChainBlockUnfinished> for crate::clvm::sexp::SExp<'static> {
    fn from(v: &RewardChainBlockUnfinished) -> Self {
        (&[
            crate::clvm::sexp::SExp::from(&v.total_iters),
            crate::clvm::sexp::SExp::from(&v.signage_point_index),
            crate::clvm::sexp::SExp::from(&v.pos_ss_cc_challenge_hash),
            crate::clvm::sexp::SExp::from(&v.proof_of_space),
            crate::clvm::sexp::SExp::from(&v.challenge_chain_sp_vdf),
            crate::clvm::sexp::SExp::from(&v.challenge_chain_sp_signature),
            crate::clvm::sexp::SExp::from(&v.reward_chain_sp_vdf),
            crate::clvm::sexp::SExp::from(&v.reward_chain_sp_signature),
        ])
            .into()
    }
}
impl From<RewardChainBlockUnfinished> for crate::clvm::sexp::SExp<'static> {
    fn from(v: RewardChainBlockUnfinished) -> Self {
        Self::from(&v)
    }
}
