use crate::blockchain::coin::Coin;
use crate::blockchain::header_block::HeaderBlock;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::spend_bundle::SpendBundle;
use crate::blockchain::tx_status::TXStatus;
use crate::clvm::program::SerializedProgram;
use dg_xch_macros::ChiaSerial;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize, parse_vec_limited};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Error};

/// The fixed wire size of a [`Bytes32`] list element — the O(1)-skip stride for the limited
/// decoders below.
const BYTES32_WIRE_SIZE: u64 = 32;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestPuzzleSolution {
    pub coin_name: Bytes32, //Min Version 0.0.34
    pub height: u32,        //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct PuzzleSolutionResponse {
    pub coin_name: Bytes32,          //Min Version 0.0.34
    pub height: u32,                 //Min Version 0.0.34
    pub puzzle: SerializedProgram,   //Min Version 0.0.34
    pub solution: SerializedProgram, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondPuzzleSolution {
    pub response: PuzzleSolutionResponse, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectPuzzleSolution {
    pub coin_name: Bytes32, //Min Version 0.0.34
    pub height: u32,        //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct SendTransaction {
    pub transaction: SpendBundle, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct TransactionAck {
    pub txid: Bytes32,         //Min Version 0.0.34
    pub status: TXStatus,      //Min Version 0.0.34
    pub error: Option<String>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct NewPeakWallet {
    pub header_hash: Bytes32,               //Min Version 0.0.34
    pub height: u32,                        //Min Version 0.0.34
    pub weight: u128,                       //Min Version 0.0.34
    pub fork_point_with_previous_peak: u32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestBlockHeader {
    pub height: u32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondBlockHeader {
    pub header_block: HeaderBlock, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectHeaderRequest {
    pub height: u32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestRemovals {
    pub height: u32,                      //Min Version 0.0.34
    pub header_hash: Bytes32,             //Min Version 0.0.34
    pub coin_names: Option<Vec<Bytes32>>, //Min Version 0.0.34
}

pub type NamedCoin = (Bytes32, Option<Coin>);
pub type NamedProofs = Option<Vec<(Bytes32, Vec<u8>)>>;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondRemovals {
    pub height: u32,           //Min Version 0.0.34
    pub header_hash: Bytes32,  //Min Version 0.0.34
    pub coins: Vec<NamedCoin>, //Min Version 0.0.34
    pub proofs: NamedProofs,   //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectRemovalsRequest {
    pub height: u32,          //Min Version 0.0.34
    pub header_hash: Bytes32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestAdditions {
    pub height: u32,                         //Min Version 0.0.34
    pub header_hash: Option<Bytes32>,        //Min Version 0.0.34
    pub puzzle_hashes: Option<Vec<Bytes32>>, //Min Version 0.0.35
}

pub type Proofs = Option<Vec<(Bytes32, Vec<u8>, Option<Vec<u8>>)>>;
pub type Additions = Vec<(Bytes32, Vec<Coin>)>;

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondAdditions {
    pub height: u32,          //Min Version 0.0.34
    pub header_hash: Bytes32, //Min Version 0.0.34
    pub coins: Additions,     //Min Version 0.0.34
    pub proofs: Proofs,       //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectAdditionsRequest {
    pub height: u32,          //Min Version 0.0.34
    pub header_hash: Bytes32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondBlockHeaders {
    pub start_height: u32,               //Min Version 0.0.34
    pub end_height: u32,                 //Min Version 0.0.34
    pub header_blocks: Vec<HeaderBlock>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectBlockHeaders {
    pub start_height: u32, //Min Version 0.0.34
    pub end_height: u32,   //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestBlockHeaders {
    pub start_height: u32,   //Min Version 0.0.34
    pub end_height: u32,     //Min Version 0.0.34
    pub return_filter: bool, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestHeaderBlocks {
    pub start_height: u32, //Min Version 0.0.34
    pub end_height: u32,   //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectHeaderBlocks {
    pub start_height: u32, //Min Version 0.0.34
    pub end_height: u32,   //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondHeaderBlocks {
    pub start_height: u32,               //Min Version 0.0.34
    pub end_height: u32,                 //Min Version 0.0.34
    pub header_blocks: Vec<HeaderBlock>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RegisterForPhUpdates {
    pub puzzle_hashes: Vec<Bytes32>, //Min Version 0.0.34
    pub min_height: u32,             //Min Version 0.0.34
}

impl RegisterForPhUpdates {
    /// Decode with `puzzle_hashes` truncated at `max_subscriptions(peer)` DURING parse.
    /// Elements past the cap are skipped in O(1), never parsed, so an inflated list claim cannot
    /// buy parse CPU; the fields after the list decode from the exact post-skip position.
    pub fn from_bytes_limited(
        bytes: &mut Cursor<&[u8]>,
        version: ChiaProtocolVersion,
        max_puzzle_hashes: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            puzzle_hashes: parse_vec_limited(
                bytes,
                version,
                max_puzzle_hashes,
                Some(BYTES32_WIRE_SIZE),
            )?,
            min_height: u32::from_bytes(bytes, version)?,
        })
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondToPhUpdates {
    pub puzzle_hashes: Vec<Bytes32>, //Min Version 0.0.34
    pub min_height: u32,             //Min Version 0.0.34
    pub coin_states: Vec<CoinState>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CoinState {
    pub coin: Coin,                  //Min Version 0.0.34
    pub spent_height: Option<u32>,   //Min Version 0.0.34
    pub created_height: Option<u32>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RegisterForCoinUpdates {
    pub coin_ids: Vec<Bytes32>, //Min Version 0.0.34
    pub min_height: u32,        //Min Version 0.0.34
}

impl RegisterForCoinUpdates {
    /// Decode with `coin_ids` truncated at `max_subscriptions(peer)` DURING parse. See
    /// [`RegisterForPhUpdates::from_bytes_limited`].
    pub fn from_bytes_limited(
        bytes: &mut Cursor<&[u8]>,
        version: ChiaProtocolVersion,
        max_coin_ids: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            coin_ids: parse_vec_limited(bytes, version, max_coin_ids, Some(BYTES32_WIRE_SIZE))?,
            min_height: u32::from_bytes(bytes, version)?,
        })
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondToCoinUpdates {
    pub coin_ids: Vec<Bytes32>,      //Min Version 0.0.34
    pub min_height: u32,             //Min Version 0.0.34
    pub coin_states: Vec<CoinState>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CoinStateUpdate {
    pub height: u32,           //Min Version 0.0.34
    pub fork_height: u32,      //Min Version 0.0.34
    pub peak_hash: Bytes32,    //Min Version 0.0.34
    pub items: Vec<CoinState>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestChildren {
    pub coin_name: Bytes32, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondChildren {
    pub coin_states: Vec<CoinState>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestSESInfo {
    pub start_height: u32, //Min Version 0.0.34
    pub end_height: u32,   //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondSESInfo {
    pub reward_chain_hash: Vec<Bytes32>, //Min Version 0.0.34
    pub heights: Vec<Vec<u32>>,          //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestFeeEstimates {
    pub time_targets: Vec<u64>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondFeeEstimates {
    pub estimates: FeeEstimateGroup, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FeeEstimate {
    pub error: Option<String>,       //Min Version 0.0.34
    pub time_target: u64,            //Min Version 0.0.34
    pub estimated_fee_rate: FeeRate, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FeeEstimateGroup {
    pub error: Option<String>,       //Min Version 0.0.34
    pub estimates: Vec<FeeEstimate>, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct FeeRate {
    pub mojos_per_clvm_cost: u64, //Min Version 0.0.34
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestRemovePuzzleSubscriptions {
    pub puzzle_hashes: Option<Vec<Bytes32>>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondRemovePuzzleSubscriptions {
    pub puzzle_hashes: Vec<Bytes32>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestRemoveCoinSubscriptions {
    pub coin_ids: Option<Vec<Bytes32>>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondRemoveCoinSubscriptions {
    pub coin_ids: Vec<Bytes32>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct CoinStateFilters {
    pub include_spent: bool,
    pub include_unspent: bool,
    pub include_hinted: bool,
    pub min_amount: u64,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestPuzzleState {
    pub puzzle_hashes: Vec<Bytes32>,
    pub previous_height: Option<u32>,
    pub header_hash: Bytes32,
    pub filters: CoinStateFilters,
    pub subscribe_when_finished: bool,
}

impl RequestPuzzleState {
    /// Decode with `puzzle_hashes` truncated at `MAX_PUZZLE_HASH_BATCH_SIZE` DURING parse. See
    /// [`RegisterForPhUpdates::from_bytes_limited`].
    pub fn from_bytes_limited(
        bytes: &mut Cursor<&[u8]>,
        version: ChiaProtocolVersion,
        max_puzzle_hashes: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            puzzle_hashes: parse_vec_limited(
                bytes,
                version,
                max_puzzle_hashes,
                Some(BYTES32_WIRE_SIZE),
            )?,
            previous_height: Option::<u32>::from_bytes(bytes, version)?,
            header_hash: Bytes32::from_bytes(bytes, version)?,
            filters: CoinStateFilters::from_bytes(bytes, version)?,
            subscribe_when_finished: bool::from_bytes(bytes, version)?,
        })
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondPuzzleState {
    pub puzzle_hashes: Vec<Bytes32>,
    pub height: u32,
    pub header_hash: Bytes32,
    pub is_finished: bool,
    pub coin_states: Vec<CoinState>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectPuzzleState {
    pub reason: u8, //RejectStateReason
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestCoinState {
    pub coin_ids: Vec<Bytes32>,
    pub previous_height: Option<u32>,
    pub header_hash: Bytes32,
    pub subscribe: bool,
}

impl RequestCoinState {
    /// Decode with `coin_ids` truncated at `max_subscribe_response_items(peer)` DURING parse.
    /// This is the worst case for the unbounded form: a `RequestCoinState` claiming 1.2M coin_ids
    /// costs seconds of parse CPU on a small node. See
    /// [`RegisterForPhUpdates::from_bytes_limited`].
    pub fn from_bytes_limited(
        bytes: &mut Cursor<&[u8]>,
        version: ChiaProtocolVersion,
        max_coin_ids: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            coin_ids: parse_vec_limited(bytes, version, max_coin_ids, Some(BYTES32_WIRE_SIZE))?,
            previous_height: Option::<u32>::from_bytes(bytes, version)?,
            header_hash: Bytes32::from_bytes(bytes, version)?,
            subscribe: bool::from_bytes(bytes, version)?,
        })
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondCoinState {
    pub coin_ids: Vec<Bytes32>,
    pub coin_states: Vec<CoinState>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RejectCoinState {
    pub reason: RejectStateReason,
}

#[derive(ChiaSerial, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum RejectStateReason {
    UNKNOWN = 255,
    REORG = 0,
    ExceededSubscriptionLimit = 1,
}
impl From<u8> for RejectStateReason {
    fn from(value: u8) -> Self {
        if value == 0 {
            RejectStateReason::REORG
        } else if value == 1 {
            RejectStateReason::ExceededSubscriptionLimit
        } else {
            RejectStateReason::UNKNOWN
        }
    }
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RemovedMempoolItem {
    pub transaction_id: Bytes32,
    pub reason: u8, //MempoolRemoveReason
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct MempoolItemsAdded {
    pub transaction_ids: Vec<Bytes32>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct MempoolItemsRemoved {
    pub removed_items: Vec<RemovedMempoolItem>,
}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RequestCostInfo {}

#[derive(ChiaSerial, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct RespondCostInfo {
    pub max_transaction_cost: u64,
    pub max_block_cost: u64,
    pub max_mempool_cost: u64,
    pub mempool_cost: u64,
    pub mempool_fee: u64,
    pub bump_fee_per_cost: u8,
}
