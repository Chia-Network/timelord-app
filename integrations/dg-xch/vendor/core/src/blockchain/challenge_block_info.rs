// Modified by Chia Network for timelord-app; see integrations/dg-xch/VENDORING.md
use super::proof_encoding::ProofEncoding;
use crate::blockchain::proof_of_space::ProofOfSpace;
use crate::blockchain::sized_bytes::{Bytes32, Bytes96};
use crate::blockchain::vdf_info::VdfInfo;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::class_group_element::ClassgroupElement;
    use crate::blockchain::proof_of_space::ProofBytes;
    use crate::blockchain::sized_bytes::Bytes48;

    fn sample() -> ChallengeBlockInfo {
        let output = ClassgroupElement::get_default_element();
        ChallengeBlockInfo {
            proof_of_space: ProofOfSpace::v1(
                Bytes32::default(),
                Some(Bytes48::default()),
                None,
                Bytes48::default(),
                32,
                ProofBytes::from(vec![0x80]),
            ),
            challenge_chain_sp_vdf: Some(VdfInfo::new(Bytes32::default(), 7, output)),
            challenge_chain_sp_signature: Bytes96::default(),
            challenge_chain_ip_vdf: VdfInfo::new(Bytes32::default(), 11, output),
        }
    }

    #[test]
    fn wire_fields_match_derived_to_bytes() {
        let value = sample();
        let version = ChiaProtocolVersion::default();
        let mut fields = Vec::new();
        value
            .append_fields(&mut fields, version, ProofEncoding::Wire)
            .unwrap();
        assert_eq!(fields, value.to_bytes(version).unwrap());
    }
}
