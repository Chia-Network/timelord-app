use super::proof_encoding::ProofEncoding;
use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ChallengeBlockInfo {
    pub proof_of_space: ProofOfSpace,
    pub challenge_chain_sp_vdf: Option<VdfInfo>,
    pub challenge_chain_sp_signature: Bytes96,
    pub challenge_chain_ip_vdf: VdfInfo,
}

impl ChallengeBlockInfo {
    fn append_fields(
        &self,
        bytes: &mut Vec<u8>,
        version: ChiaProtocolVersion,
        encoding: ProofEncoding,
    ) -> Result<(), Error> {
        encoding.append(&self.proof_of_space, bytes, version)?;
        self.challenge_chain_sp_vdf.append_bytes(bytes, version)?;
        self.challenge_chain_sp_signature
            .append_bytes(bytes, version)?;
        self.challenge_chain_ip_vdf.append_bytes(bytes, version)?;
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

impl ChiaSerialize for ChallengeBlockInfo {
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
            proof_of_space: ChiaSerialize::from_bytes(bytes, version)?,
            challenge_chain_sp_vdf: ChiaSerialize::from_bytes(bytes, version)?,
            challenge_chain_sp_signature: ChiaSerialize::from_bytes(bytes, version)?,
            challenge_chain_ip_vdf: ChiaSerialize::from_bytes(bytes, version)?,
        })
    }
}
impl From<&ChallengeBlockInfo> for crate::clvm::sexp::SExp<'static> {
    fn from(v: &ChallengeBlockInfo) -> Self {
        (&[
            crate::clvm::sexp::SExp::from(&v.proof_of_space),
            crate::clvm::sexp::SExp::from(&v.challenge_chain_sp_vdf),
            crate::clvm::sexp::SExp::from(&v.challenge_chain_sp_signature),
            crate::clvm::sexp::SExp::from(&v.challenge_chain_ip_vdf),
        ])
            .into()
    }
}
impl From<ChallengeBlockInfo> for crate::clvm::sexp::SExp<'static> {
    fn from(v: ChallengeBlockInfo) -> Self {
        Self::from(&v)
    }
}
