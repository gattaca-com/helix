use alloy_primitives::B256;
use helix_types::BlsPublicKey;

use crate::{
    error::AuctioneerError,
    types::keys::{
        BID_FLOOR_KEY, BID_FLOOR_VALUE_KEY, BID_TRACE_KEY, BLOCK_BUILDER_LATEST_BID_KEY,
        BLOCK_BUILDER_LATEST_BID_TIME_KEY, BLOCK_BUILDER_LATEST_BID_VALUE_KEY, EXEC_PAYLOAD_KEY,
        GET_HEADER_RESPONSE_KEY, HEADER_TX_ROOT, PENDING_BLOCK_KEY, SEEN_BLOCK_HASHES_KEY,
        TOP_BID_VALUE_KEY,
    },
};

pub fn get_cache_get_header_response_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{GET_HEADER_RESPONSE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_execution_payload_key(
    slot: u64,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &B256,
) -> String {
    format!("{EXEC_PAYLOAD_KEY}:{slot}_{proposer_pub_key:?}_{block_hash:?}")
}

pub fn get_cache_bid_trace_key(
    slot: u64,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &B256,
) -> String {
    format!("{BID_TRACE_KEY}:{slot}_{proposer_pub_key:?}_{block_hash:?}")
}

pub fn get_latest_bid_by_builder_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
    builder_pub_key: &BlsPublicKey,
) -> String {
    format!(
        "{BLOCK_BUILDER_LATEST_BID_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}/{builder_pub_key:?}"
    )
}

pub fn get_latest_bid_by_builder_key_str_builder_pub_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
    builder_pub_key: &str,
) -> String {
    format!(
        "{BLOCK_BUILDER_LATEST_BID_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}/{builder_pub_key}"
    )
}

pub fn get_builder_latest_bid_value_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BLOCK_BUILDER_LATEST_BID_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_builder_latest_bid_time_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BLOCK_BUILDER_LATEST_BID_TIME_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_top_bid_value_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{TOP_BID_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_floor_bid_key(slot: u64, parent_hash: &B256, proposer_pub_key: &BlsPublicKey) -> String {
    format!("{BID_FLOOR_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_floor_bid_value_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BID_FLOOR_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_seen_block_hashes_key(
    slot: u64,
    parent_hash: &B256,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &B256,
) -> String {
    format!("{SEEN_BLOCK_HASHES_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}_{block_hash:?}")
}

pub fn get_pending_block_builder_key(builder_pub_key: &BlsPublicKey) -> String {
    format!("{PENDING_BLOCK_KEY}:{builder_pub_key:?}")
}

pub fn get_pending_block_builder_block_hash_key(
    builder_pub_key: &BlsPublicKey,
    block_hash: &B256,
) -> String {
    format!("{PENDING_BLOCK_KEY}:{builder_pub_key:?}_{block_hash:?}")
}

pub fn get_pubkey_from_hex(pubkey: &str) -> Result<BlsPublicKey, AuctioneerError> {
    let bytes = alloy_primitives::hex::decode(pubkey)?;
    let pubkey =
        BlsPublicKey::deserialize(bytes.as_slice()).map_err(AuctioneerError::CryptoError)?;
    Ok(pubkey)
}

pub fn get_hash_from_hex(hash: &str) -> Result<B256, AuctioneerError> {
    let bytes = alloy_primitives::hex::decode(hash)?;
    Ok(B256::try_from(bytes.as_slice())?)
}

pub fn get_header_tx_root_key(hash: &B256) -> String {
    format!("{HEADER_TX_ROOT}:{hash:?}")
}
