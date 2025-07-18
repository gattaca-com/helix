use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    bid_submission::v2::header_submission::SignedHeaderSubmission, simulator::BlockSimError,
    GossipedPayloadTrace, HeaderSubmissionTrace, SubmissionTrace,
};
use helix_types::{BlsPublicKey, SignedBidSubmission};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>, SubmissionTrace, OptimisticVersion),
    NewHeaderSubmission(Arc<SignedHeaderSubmission>, HeaderSubmissionTrace),
    GossipedPayload { block_hash: B256, trace: GossipedPayloadTrace },
    SimulationResult { block_hash: B256, block_sim_result: Result<(), BlockSimError> },
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
}

#[derive(Debug, Deserialize)]
pub struct InclusionListPathParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pub_key: BlsPublicKey,
}
