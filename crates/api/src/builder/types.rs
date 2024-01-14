use std::sync::Arc;

use helix_common::{bid_submission::{SignedBidSubmission, v2::header_submission::SignedHeaderSubmission}, bellatrix::ByteVector, simulator::BlockSimError, HeaderSubmissionTrace, GossipedHeaderTrace, GossipedPayloadTrace};

pub const PATH_BUILDER_API: &str = "/relay/v1/builder";

pub const PATH_GET_VALIDATORS: &str = "/validators";
pub const PATH_SUBMIT_BLOCK: &str = "/blocks";
pub const PATH_SUBMIT_BLOCK_OPTIMISTIC: &str = "/blocks_optimistic";
pub const PATH_SUBMIT_HEADER: &str = "/headers";


#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>),
    NewHeaderSubmission(Arc<SignedHeaderSubmission>, Arc<HeaderSubmissionTrace>),
    GossipedHeader { block_hash: ByteVector<32>, trace: Arc<GossipedHeaderTrace> },
    GossipedPayload { block_hash: ByteVector<32>, trace: Arc<GossipedPayloadTrace> },
    SimulationResult { block_hash: ByteVector<32>, block_sim_result: Result<(), BlockSimError> },
}