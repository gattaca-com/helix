pub mod multi_simulator;
pub mod optimistic_simulator;

#[cfg(test)]
mod optimistic_simulator_tests;

pub mod rpc_simulator;

#[cfg(test)]
mod simulator_tests;

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    api::builder_api::InclusionListWithMetadata, bid_submission::BidSubmission,
    ValidatorPreferences,
};
use helix_types::{
    BidTrace, BlobsBundle, BlsSignatureBytes, ExecutionPayload, ExecutionRequests,
    MergeableOrderWithOrigin, SignedBidSubmission,
};
use serde_json::json;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSimRequest {
    #[serde(with = "serde_utils::quoted_u64")]
    pub registered_gas_limit: u64,
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignatureBytes,
    pub proposer_preferences: ValidatorPreferences,
    pub blobs_bundle: Option<Arc<BlobsBundle>>,
    pub execution_requests: Option<Arc<ExecutionRequests>>,
    pub parent_beacon_block_root: Option<B256>,
    pub inclusion_list: Option<InclusionListWithMetadata>,
    pub apply_blacklist: bool,
}

impl BlockSimRequest {
    pub fn new(
        registered_gas_limit: u64,
        block: &SignedBidSubmission,
        proposer_preferences: ValidatorPreferences,
        parent_beacon_block_root: Option<B256>,
        inclusion_list: Option<InclusionListWithMetadata>,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace().clone(),
            execution_payload: block.execution_payload_ref().clone(),
            signature: *block.signature(),
            apply_blacklist: proposer_preferences.filtering.is_regional(),
            proposer_preferences,
            blobs_bundle: Some(block.blobs_bundle().clone()),
            execution_requests: block.execution_requests(),
            parent_beacon_block_root,
            inclusion_list,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockMergeRequestRef<'a> {
    /// The original payload value
    pub original_value: U256,
    pub proposer_fee_recipient: Address,
    pub execution_payload: &'a ExecutionPayload,
    pub parent_beacon_block_root: Option<B256>,
    pub merging_data: &'a [MergeableOrderWithOrigin],
}

pub struct BlockMergeRequest {
    /// The serialized request
    request: serde_json::Value,
    /// The block hash of the execution payload
    block_hash: B256,
}

impl BlockMergeRequest {
    pub fn new(
        original_value: U256,
        proposer_fee_recipient: Address,
        execution_payload: &ExecutionPayload,
        parent_beacon_block_root: Option<B256>,
        merging_data: &[MergeableOrderWithOrigin],
    ) -> Self {
        let block_hash = execution_payload.block_hash;
        // We serialize the request ahead of time, to avoid copying the original
        // payload and merging data.
        let request_ref = BlockMergeRequestRef {
            original_value,
            proposer_fee_recipient,
            execution_payload,
            parent_beacon_block_root,
            merging_data,
        };
        let request = json!(request_ref);
        Self { request, block_hash }
    }
}
