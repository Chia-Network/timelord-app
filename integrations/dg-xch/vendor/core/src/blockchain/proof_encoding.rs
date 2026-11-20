// Added by Chia Network for timelord-app; see integrations/dg-xch/VENDORING.md
use super::proof_of_space::ProofOfSpace;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Error;
/// Only the proof representation differs between wire and consensus field traversal.
#[derive(Copy, Clone)]
pub(super) enum ProofEncoding {
    Wire,
    Commitment,
}
impl ProofEncoding {
    pub(super) fn append(
        self,
        proof: &ProofOfSpace,
        bytes: &mut Vec<u8>,
        version: ChiaProtocolVersion,
    ) -> Result<(), Error> {
        match self {
            Self::Wire => proof.append_bytes(bytes, version),
            Self::Commitment => {
                bytes.extend(proof.consensus_bytes(version)?);
                Ok(())
            }
        }
    }
}
