use crate::blockchain::coin::Coin;
use crate::blockchain::coin_spend::CoinSpend;
use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::program::Program;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CoinRecord {
    pub coin: Coin,
    pub confirmed_block_index: u32,
    pub spent_block_index: u32,
    pub coinbase: bool,
    pub timestamp: u64,
    pub spent: bool,
}

/// The newest unspent version of a singleton, addressed by puzzle hash. The mempool
/// rebases a fast-forward-eligible spend of an older singleton version onto this coin.
#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct UnspentLineageInfo {
    /// The unspent singleton coin's id.
    pub coin_id: Bytes32,
    /// Its parent's coin id (the parent shares the singleton's puzzle hash and amount).
    pub parent_id: Bytes32,
    /// The parent's parent coin id — the rebased lineage proof's `parent_parent_coin_info`.
    pub parent_parent_id: Bytes32,
}

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum CatVersion {
    V1,
    V2,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CatCoinRecord {
    pub delegate: CoinRecord,
    pub version: CatVersion,
    pub asset_id: Bytes32,
    #[serde(borrow)]
    pub cat_program: Program<'static>,
    #[serde(borrow)]
    pub lineage_proof: Program<'static>,
    pub parent_coin_spend: CoinSpend,
}

//Not Standard Protocol
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct PaginatedCoinRecord {
    pub coin: Coin,
    pub coin_spend: Option<CoinSpend>,
    pub parent_coin_spend: Option<CoinSpend>,
    pub confirmed_block_index: u32,
    pub spent_block_index: u32,
    pub timestamp: u64,
    pub coinbase: bool,
    pub spent: bool,
}

//Not Standard Protocol
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct HintedCoinRecord {
    pub coin: Coin,
    pub parent_coin_spend: Option<CoinSpend>,
    pub confirmed_block_index: u32,
    pub spent_block_index: u32,
    pub timestamp: u64,
    pub coinbase: bool,
    pub spent: bool,
    pub hint: Option<Bytes32>,
}
