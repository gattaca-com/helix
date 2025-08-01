pub mod multi_simulator;
pub mod optimistic_simulator;

#[cfg(test)]
mod optimistic_simulator_tests;

pub mod rpc_simulator;

#[cfg(test)]
mod simulator_tests;

use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::InclusionListWithMetadata, bid_submission::BidSubmission,
    ValidatorPreferences,
};
use helix_types::{
    BidTrace, BlobsBundle, BlsSignature, ExecutionPayload, ExecutionRequests, SignedBidSubmission,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSimRequest {
    #[serde(with = "serde_utils::quoted_u64")]
    pub registered_gas_limit: u64,
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
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
            execution_payload: block.execution_payload_ref().clone_from_ref(),
            signature: block.signature().clone(),
            apply_blacklist: proposer_preferences.filtering.is_regional(),
            proposer_preferences,
            blobs_bundle: Some(block.blobs_bundle().clone()),
            execution_requests: block.execution_requests(),
            parent_beacon_block_root,
            inclusion_list,
        }
    }
}
