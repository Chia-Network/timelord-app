use super::proof_encoding::ProofEncoding;
use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::reward_chain_block_unfinished::RewardChainBlockUnfinished;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RewardChainBlock {
    pub weight: u128,
    pub height: u32,
    pub total_iters: u128,
    pub signage_point_index: u8,
    pub pos_ss_cc_challenge_hash: Bytes32,
    pub proof_of_space: ProofOfSpace,
    pub challenge_chain_sp_vdf: Option<VdfInfo>,
    pub challenge_chain_sp_signature: Bytes96,
    pub challenge_chain_ip_vdf: VdfInfo,
    pub reward_chain_sp_vdf: Option<VdfInfo>,
    pub reward_chain_sp_signature: Bytes96,
    pub reward_chain_ip_vdf: VdfInfo,
    pub infused_challenge_chain_ip_vdf: Option<VdfInfo>,
    pub header_mmr_root: Option<Bytes32>,
    pub is_transaction_block: bool,
}

impl RewardChainBlock {
    fn append_fields(
        &self,
        bytes: &mut Vec<u8>,
        version: ChiaProtocolVersion,
        encoding: ProofEncoding,
    ) -> Result<(), Error> {
        self.weight.append_bytes(bytes, version)?;
        self.height.append_bytes(bytes, version)?;
        self.total_iters.append_bytes(bytes, version)?;
        self.signage_point_index.append_bytes(bytes, version)?;
        self.pos_ss_cc_challenge_hash.append_bytes(bytes, version)?;
        encoding.append(&self.proof_of_space, bytes, version)?;
        self.challenge_chain_sp_vdf.append_bytes(bytes, version)?;
        self.challenge_chain_sp_signature
            .append_bytes(bytes, version)?;
        self.challenge_chain_ip_vdf.append_bytes(bytes, version)?;
        self.reward_chain_sp_vdf.append_bytes(bytes, version)?;
        self.reward_chain_sp_signature
            .append_bytes(bytes, version)?;
        self.reward_chain_ip_vdf.append_bytes(bytes, version)?;
        super::packed_optional::append(
            self.infused_challenge_chain_ip_vdf.as_ref(),
            self.header_mmr_root.as_ref(),
            bytes,
            version,
        )?;
        self.is_transaction_block.append_bytes(bytes, version)
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

    #[must_use]
    pub fn get_unfinished(&self) -> RewardChainBlockUnfinished {
        RewardChainBlockUnfinished {
            total_iters: self.total_iters,
            signage_point_index: self.signage_point_index,
            pos_ss_cc_challenge_hash: self.pos_ss_cc_challenge_hash,
            proof_of_space: self.proof_of_space.clone(),
            challenge_chain_sp_vdf: self.challenge_chain_sp_vdf,
            challenge_chain_sp_signature: self.challenge_chain_sp_signature,
            reward_chain_sp_vdf: self.reward_chain_sp_vdf,
            reward_chain_sp_signature: self.reward_chain_sp_signature,
        }
    }
}

impl ChiaSerialize for RewardChainBlock {
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
        let weight: u128 = ChiaSerialize::from_bytes(bytes, version)?;
        let height: u32 = ChiaSerialize::from_bytes(bytes, version)?;
        let total_iters: u128 = ChiaSerialize::from_bytes(bytes, version)?;
        let signage_point_index: u8 = ChiaSerialize::from_bytes(bytes, version)?;
        let pos_ss_cc_challenge_hash: Bytes32 = ChiaSerialize::from_bytes(bytes, version)?;
        let proof_of_space: ProofOfSpace = ChiaSerialize::from_bytes(bytes, version)?;
        let challenge_chain_sp_vdf: Option<VdfInfo> = ChiaSerialize::from_bytes(bytes, version)?;
        let challenge_chain_sp_signature: Bytes96 = ChiaSerialize::from_bytes(bytes, version)?;
        let challenge_chain_ip_vdf: VdfInfo = ChiaSerialize::from_bytes(bytes, version)?;
        let reward_chain_sp_vdf: Option<VdfInfo> = ChiaSerialize::from_bytes(bytes, version)?;
        let reward_chain_sp_signature: Bytes96 = ChiaSerialize::from_bytes(bytes, version)?;
        let reward_chain_ip_vdf: VdfInfo = ChiaSerialize::from_bytes(bytes, version)?;
        let (infused_challenge_chain_ip_vdf, header_mmr_root) =
            super::packed_optional::parse(bytes, version)?;
        let is_transaction_block = ChiaSerialize::from_bytes(bytes, version)?;
        Ok(Self {
            weight,
            height,
            total_iters,
            signage_point_index,
            pos_ss_cc_challenge_hash,
            proof_of_space,
            challenge_chain_sp_vdf,
            challenge_chain_sp_signature,
            challenge_chain_ip_vdf,
            reward_chain_sp_vdf,
            reward_chain_sp_signature,
            reward_chain_ip_vdf,
            infused_challenge_chain_ip_vdf,
            header_mmr_root,
            is_transaction_block,
        })
    }
}

impl From<&RewardChainBlock> for crate::clvm::sexp::SExp<'static> {
    fn from(val: &RewardChainBlock) -> Self {
        (&[
            crate::clvm::sexp::SExp::from(&val.weight),
            crate::clvm::sexp::SExp::from(&val.height),
            crate::clvm::sexp::SExp::from(&val.total_iters),
            crate::clvm::sexp::SExp::from(&val.signage_point_index),
            crate::clvm::sexp::SExp::from(&val.pos_ss_cc_challenge_hash),
            crate::clvm::sexp::SExp::from(&val.proof_of_space),
            crate::clvm::sexp::SExp::from(&val.challenge_chain_sp_vdf),
            crate::clvm::sexp::SExp::from(&val.challenge_chain_sp_signature),
            crate::clvm::sexp::SExp::from(&val.challenge_chain_ip_vdf),
            crate::clvm::sexp::SExp::from(&val.reward_chain_sp_vdf),
            crate::clvm::sexp::SExp::from(&val.reward_chain_sp_signature),
            crate::clvm::sexp::SExp::from(&val.reward_chain_ip_vdf),
            crate::clvm::sexp::SExp::from(&val.infused_challenge_chain_ip_vdf),
            crate::clvm::sexp::SExp::from(&val.header_mmr_root),
            crate::clvm::sexp::SExp::from(&val.is_transaction_block),
        ])
            .into()
    }
}
impl From<RewardChainBlock> for crate::clvm::sexp::SExp<'static> {
    fn from(val: RewardChainBlock) -> Self {
        Self::from(&val)
    }
}
