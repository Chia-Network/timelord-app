use crate::blockchain::coin::Coin;
use crate::blockchain::sized_bytes::Bytes32;
use crate::traits::SizedBytes;

/// Pool parent id: `genesis_challenge[..16] || block_height as 16-byte big-endian`. Because
/// `block_height` is a `u32`, its 16-byte big-endian form is 12 zero bytes followed by 4
/// height bytes, so the height's 4 bytes land at `[28..32]` over a zeroed buffer.
#[must_use]
pub fn pool_parent_id(block_height: u32, genesis_challenge: Bytes32) -> Bytes32 {
    let mut buf: [u8; 32] = [0; 32];
    buf[0..16].copy_from_slice(&genesis_challenge[0..16]);
    buf[28..32].copy_from_slice(&block_height.to_be_bytes());
    Bytes32::new(buf)
}

/// Farmer parent id: `genesis_challenge[16..] || block_height as 16-byte big-endian` — the
/// challenge's *high* 16 bytes (vs the pool prefix's low 16), so pool and farmer parent ids
/// never collide. Same 16-byte big-endian height layout as [`pool_parent_id`].
#[must_use]
pub fn farmer_parent_id(block_height: u32, genesis_challenge: Bytes32) -> Bytes32 {
    let mut buf: [u8; 32] = [0; 32];
    buf[0..16].copy_from_slice(&genesis_challenge[16..32]);
    buf[28..32].copy_from_slice(&block_height.to_be_bytes());
    Bytes32::new(buf)
}

/// `Coin(pool_parent_id(...), puzzle_hash, reward)`.
#[must_use]
pub fn create_pool_coin(
    block_height: u32,
    puzzle_hash: Bytes32,
    amount: u64,
    genesis_challenge: Bytes32,
) -> Coin {
    let parent_coin_info = pool_parent_id(block_height, genesis_challenge);
    Coin {
        parent_coin_info,
        puzzle_hash,
        amount,
    }
}

/// `Coin(farmer_parent_id(...), puzzle_hash, reward)`.
#[must_use]
pub fn create_farmer_coin(
    block_height: u32,
    puzzle_hash: Bytes32,
    amount: u64,
    genesis_challenge: Bytes32,
) -> Coin {
    let parent_coin_info = farmer_parent_id(block_height, genesis_challenge);
    Coin {
        parent_coin_info,
        puzzle_hash,
        amount,
    }
}
