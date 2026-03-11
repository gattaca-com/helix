use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    SubmissionTrace,
    bid_submission::OptimisticVersion,
    simulator::{BlockSimError, SimRequest},
};
use helix_types::{
    BlsPublicKeyBytes, BuilderInclusionResult, ExecutionPayload, ExecutionRequests,
    MergeableOrderWithOrigin, MergedBlockTrace, SignedBidSubmission, SubmissionVersion,
};

use crate::auctioneer::SubmissionRef;

pub mod client;
pub mod tile;

pub use tile::SimulatorTile;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockMergeRequestRef<'a> {
    /// The original payload value
    pub original_value: U256,
    pub proposer_fee_recipient: Address,
    pub execution_payload: &'a ExecutionPayload,
    pub parent_beacon_block_root: Option<B256>,
    pub merging_data: &'a [MergeableOrderWithOrigin],
    pub trace: MergedBlockTrace,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockMergeRequest {
    pub bid_slot: u64,
    /// The serialized request
    pub request: serde_json::Value,
    /// The block hash of the execution payload
    pub block_hash: B256,
}

pub type BlockMergeResult = (usize, Result<BlockMergeResponse, BlockSimError>);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockMergeResponse {
    pub base_block_hash: B256,
    pub execution_payload: ExecutionPayload,
    pub execution_requests: ExecutionRequests,
    /// Versioned hashes of the appended blob transactions.
    pub appended_blobs: Vec<B256>,
    /// Total value for the proposer
    pub proposer_value: U256,
    pub builder_inclusions: HashMap<Address, BuilderInclusionResult>,
    pub trace: MergedBlockTrace,
}

#[derive(Clone)]
pub struct SimulatorRequest {
    pub is_optimistic: bool,
    pub request: SimRequest,
    pub is_top_bid: bool,
    pub bid_slot: u64,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub version: SubmissionVersion,
    pub submission: SignedBidSubmission,
    pub submission_ref: SubmissionRef,
    pub trace: SubmissionTrace,
    // only Some for dehydrated submissions
    pub tx_root: Option<B256>,
}

/// Large payload stored in `SharedVector` for auctioneer → sim tile transfer.
pub enum SimInboundPayload {
    SimRequest { req: Box<SimulatorRequest>, fast_track: bool },
    MergeRequest(BlockMergeRequest),
}

/// Large payload stored in `SharedVector` for sim tile → auctioneer transfer.
pub enum SimOutboundPayload {
    SimResult(crate::simulator::tile::SimulationResult),
    MergeResult(BlockMergeResult),
}

impl SimulatorRequest {
    pub fn on_receive_ns(&self) -> u64 {
        self.trace.receive_ns.0
    }

    // TODO: use a "score" eg how close to top bid even if below
    pub fn sort_key(&self) -> (u8, u64) {
        let top = if self.is_top_bid { 1 } else { 0 };
        (top, u64::MAX - self.on_receive_ns())
    }

    pub fn optimistic_version(&self) -> OptimisticVersion {
        if self.is_optimistic { OptimisticVersion::V1 } else { OptimisticVersion::NotOptimistic }
    }
}
