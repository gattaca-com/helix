use std::{alloc::System, sync::Arc, time::SystemTime};

use ethereum_consensus::primitives::BlsPublicKey;
use helix_common::{bellatrix::ByteVector, bid_submission::{SignedBidSubmission, v2::header_submission::SignedHeaderSubmission}, simulator::BlockSimError, GossipedHeaderTrace, GossipedPayloadTrace, HeaderSubmissionTrace, SubmissionTrace};

pub const PATH_BUILDER_API: &str = "/relay/v1/builder";

pub const PATH_GET_VALIDATORS: &str = "/validators";
pub const PATH_SUBMIT_BLOCK: &str = "/blocks";
pub const PATH_SUBMIT_BLOCK_OPTIMISTIC: &str = "/blocks_optimistic";
pub const PATH_SUBMIT_HEADER: &str = "/headers";


#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>, Arc<SubmissionTrace>, OptimisticVersion),
    NewHeaderSubmission(Arc<SignedHeaderSubmission>, Arc<HeaderSubmissionTrace>),
    PayloadReceived{ block_hash: ByteVector<32>, proposer_pubkey: BlsPublicKey, slot: u64, time: SystemTime },
    GossipedHeader { block_hash: ByteVector<32>, trace: Arc<GossipedHeaderTrace> },
    GossipedPayload { block_hash: ByteVector<32>, trace: Arc<GossipedPayloadTrace> },
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