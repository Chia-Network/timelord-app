use crate::blockchain::sized_bytes::{Bytes32, Bytes100};
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use serde::{Deserialize, Serialize};
use std::io::Error;

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ClassgroupElement {
    pub data: Bytes100,
}

impl ClassgroupElement {
    /// Returns the VDF identity element.
    #[must_use]
    pub fn get_default_element() -> Self {
        let mut bytes = [0u8; 100];
        bytes[0] = 0x08;
        Self {
            data: Bytes100::from(bytes),
        }
    }

    pub fn hash(&self) -> Result<Bytes32, Error> {
        Ok(Bytes32::from(hash_256(
            self.to_bytes(ChiaProtocolVersion::default())?,
        )))
    }
}
