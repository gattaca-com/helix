pub mod mock_simulator;
pub mod optimistic_simulator;
mod optimistic_simulator_tests;
pub mod rpc_simulator;
mod simulator_tests;
pub mod traits;

use ethereum_consensus::types::mainnet::ExecutionPayload;
use std::sync::Arc;

use ethereum_consensus::{primitives::BlsSignature, serde::as_str};
use helix_common::{
    bid_submission::{BidSubmission, BidTrace, SignedBidSubmission},
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
}

impl BlockSimRequest {
    pub fn new(
        registered_gas_limit: u64,
        block: Arc<SignedBidSubmission>,
        proposer_preferences: ValidatorPreferences,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace().clone(),
            execution_payload: block.execution_payload().clone(),
            signature: block.signature().clone(),
            proposer_preferences,
        }
    }
}
