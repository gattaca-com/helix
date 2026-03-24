use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    api::builder_api::InclusionListWithMetadata, bid_submission::OptimisticVersion,
    simulator::BlockSimError,
};
use helix_types::{
    BuilderInclusionResult, ExecutionPayload, ExecutionRequests, MergeableOrderWithOrigin,
    MergedBlockTrace,
};

use crate::simulator::tile::ValidationResult;

pub mod client;
pub mod tile;

pub use tile::SimulatorTile;

#[derive(Debug, Clone)]
pub struct ValidationRequest {
    pub is_top_bid: bool,
    pub is_optimistic: bool,
    pub apply_blacklist: bool,
    pub registered_gas_limit: u64,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: InclusionListWithMetadata,
    pub decoded_ix: usize,
    pub receive_ns: u64,
}

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
pub struct MergeRequest {
    pub bid_slot: u64,
    /// The serialized request
    pub request: serde_json::Value,
    /// The block hash of the execution payload
    pub block_hash: B256,
}

pub type MergeResult = (usize, Result<BlockMergeResponse, BlockSimError>);

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

/// Large payload stored in `SharedVector` for auctioneer → sim tile transfer.
pub enum SimRequest {
    Validate { req: Box<ValidationRequest>, fast_track: bool },
    Merge(MergeRequest),
}

/// Large payload stored in `SharedVector` for sim tile → auctioneer transfer.
pub enum SimResult {
    Validate(ValidationResult),
    Merge(MergeResult),
}

impl ValidationRequest {
    pub fn on_receive_ns(&self) -> u64 {
        self.receive_ns
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
