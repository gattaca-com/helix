use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    bid_submission::v2::header_submission::SignedHeaderSubmission, simulator::BlockSimError,
    GossipedHeaderTrace, GossipedPayloadTrace, HeaderSubmissionTrace, SubmissionTrace,
};
use helix_types::SignedBidSubmission;

#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>, SubmissionTrace, OptimisticVersion),
    NewHeaderSubmission(Arc<SignedHeaderSubmission>, HeaderSubmissionTrace),
    GossipedHeader { block_hash: B256, trace: GossipedHeaderTrace },
    GossipedPayload { block_hash: B256, trace: GossipedPayloadTrace },
    SimulationResult { block_hash: B256, block_sim_result: Result<(), BlockSimError> },
}

#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    V2 = 2,
}
