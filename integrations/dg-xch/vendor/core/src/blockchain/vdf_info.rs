use crate::blockchain::class_group_element::ClassgroupElement;
use crate::blockchain::sized_bytes::Bytes32;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct VdfInfo {
    pub challenge: Bytes32,
    pub number_of_iterations: u64,
    pub output: ClassgroupElement,
}

impl VdfInfo {
    #[must_use]
    pub fn new(challenge: Bytes32, number_of_iterations: u64, output: ClassgroupElement) -> Self {
        Self {
            challenge,
            number_of_iterations,
            output,
        }
    }

    #[must_use]
    pub fn with_iters(&self, iters: u64) -> Self {
        Self {
            number_of_iterations: iters,
            ..*self
        }
    }
}
