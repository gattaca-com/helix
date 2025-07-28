use alloy_primitives::B256;
use helix_types::BlsPublicKey;

use crate::types::keys::{BID_TRACE_KEY, BLOCK_MERGING_DATA_KEY, EXEC_PAYLOAD_KEY};

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

pub fn get_cache_block_merging_data_key(
    slot: u64,
    proposer_pub_key: &BlsPublicKey,
    block_hash: &B256,
) -> String {
    format!("{BLOCK_MERGING_DATA_KEY}:{slot}_{proposer_pub_key:?}_{block_hash:?}")
}
