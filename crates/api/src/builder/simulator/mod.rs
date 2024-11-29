pub mod mock_simulator;
pub mod optimistic_simulator;
mod optimistic_simulator_tests;
pub mod rpc_simulator;
pub mod traits;

#[cfg(test)]
mod simulator_tests;

use ethereum_consensus::{deneb::Bytes32, types::mainnet::ExecutionPayload};
use std::sync::Arc;

use ethereum_consensus::{primitives::BlsSignature, serde::as_str};
use helix_common::{
    bid_submission::{BidSubmission, BidTrace, SignedBidSubmission},
    deneb::BlobsBundle,
    ValidatorPreferences,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSimRequest {
    #[serde(with = "as_str")]
    pub registered_gas_limit: u64,
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
    pub proposer_preferences: ValidatorPreferences,
    pub blobs_bundle: Option<BlobsBundle>,
    pub parent_beacon_block_root: Option<Bytes32>,
}

impl BlockSimRequest {
    pub fn new(
        registered_gas_limit: u64,
        block: Arc<SignedBidSubmission>,
        proposer_preferences: ValidatorPreferences,
        parent_beacon_block_root: Option<Bytes32>,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace().clone(),
            execution_payload: block.execution_payload().clone(),
            signature: block.signature().clone(),
            proposer_preferences,
            blobs_bundle: block.blobs_bundle().cloned(),
            parent_beacon_block_root,
        }
    }
}
