use crate::blockchain::condition_with_args::Message;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::unsized_bytes::UnsizedBytes;
use crate::utils::hash_256;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct Announcement {
    pub origin_info: Bytes32,
    pub message: Message,
    pub morph_bytes: Option<UnsizedBytes>,
}
impl Announcement {
    #[must_use]
    pub fn name(&self) -> Bytes32 {
        let mut buf = vec![];
        buf.extend(self.origin_info);
        match &self.morph_bytes {
            Some(m) => {
                let mut morph_buf = vec![];
                morph_buf.extend(&m.bytes);
                morph_buf.extend(self.message.data());
                buf.extend(hash_256(morph_buf));
            }
            None => buf.extend(self.message.data()),
        };
        hash_256(buf).into()
    }
}
