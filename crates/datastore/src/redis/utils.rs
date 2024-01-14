use ethereum_consensus::primitives::{BlsPublicKey, Hash32};

use crate::types::keys::{
    BID_FLOOR_KEY, BID_FLOOR_VALUE_KEY, BID_TRACE_KEY, BLOCK_BUILDER_LATEST_BID_KEY,
    BLOCK_BUILDER_LATEST_BID_TIME_KEY, BLOCK_BUILDER_LATEST_BID_VALUE_KEY, EXEC_PAYLOAD_KEY,
    GET_HEADER_RESPONSE_KEY, TOP_BID_VALUE_KEY, SEEN_BLOCK_HASHES_KEY,
};

pub fn get_cache_get_header_response_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{GET_HEADER_RESPONSE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_execution_payload_key(
    slot: u64,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &Hash32,
) -> String {
    format!("{EXEC_PAYLOAD_KEY}:{slot}_{proposer_pub_key:?}_{block_hash:?}")
}

pub fn get_cache_bid_trace_key(
    slot: u64,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &Hash32,
) -> String {
    format!("{BID_TRACE_KEY}:{slot}_{proposer_pub_key:?}_{block_hash:?}")
}

pub fn get_latest_bid_by_builder_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
    builder_pub_key: &BlsPublicKey,
) -> String {
    format!(
        "{BLOCK_BUILDER_LATEST_BID_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}/{builder_pub_key:?}"
    )
}

pub fn get_latest_bid_by_builder_key_str_builder_pub_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
    builder_pub_key: &str,
) -> String {
    format!(
        "{BLOCK_BUILDER_LATEST_BID_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}/{builder_pub_key}"
    )
}

pub fn get_builder_latest_bid_value_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BLOCK_BUILDER_LATEST_BID_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_builder_latest_bid_time_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BLOCK_BUILDER_LATEST_BID_TIME_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_top_bid_value_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{TOP_BID_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_floor_bid_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BID_FLOOR_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_floor_bid_value_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{BID_FLOOR_VALUE_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

pub fn get_seen_block_hashes_key(
    slot: u64,
    parent_hash: &Hash32,
    proposer_pub_key: &BlsPublicKey,
) -> String {
    format!("{SEEN_BLOCK_HASHES_KEY}:{slot}_{parent_hash:?}_{proposer_pub_key:?}")
}

