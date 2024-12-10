use std::sync::Arc;

use helix_common::{
    bellatrix::ByteVector,
    bid_submission::{v2::header_submission::SignedHeaderSubmission, SignedBidSubmission},
    simulator::BlockSimError,
    GossipedHeaderTrace, GossipedPayloadTrace, HeaderSubmissionTrace, SubmissionTrace,
};

#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>, SubmissionTrace, OptimisticVersion),
    NewHeaderSubmission(Arc<SignedHeaderSubmission>, HeaderSubmissionTrace),
    GossipedHeader { block_hash: ByteVector<32>, trace: GossipedHeaderTrace },
    GossipedPayload { block_hash: ByteVector<32>, trace: GossipedPayloadTrace },
    SimulationResult { block_hash: ByteVector<32>, block_sim_result: Result<(), BlockSimError> },
}

#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    V2 = 2,
}
