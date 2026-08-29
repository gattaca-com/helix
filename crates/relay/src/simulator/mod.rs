use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    api::builder_api::InclusionListWithMetadata, bid_submission::OptimisticVersion,
    simulator::BlockSimError,
};
use helix_types::{
    BlobsBundle, BuilderInclusionResult, ExecutionPayload, ExecutionRequests, MergedBlockTrace,
};

use crate::{
    SubmissionRef,
    simulator::tile::{MergedSimulationResult, ValidationResult},
};

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
    pub submission_ref: SubmissionRef,
}

pub type MergeResult = (usize, Result<BlockMergeResponse, BlockSimError>);

/// Simulation of an incoming merged block from the merge builder. Unlike `ValidationRequest`,
/// there's no decoded bid submission to look up: the block itself lives in `merged_blocks`,
/// indexed by `merged_block_ix`.
#[derive(Debug, Clone)]
pub struct MergedValidationRequest {
    pub merged_block_ix: usize,
    /// Kept alongside the index for `PendingMergeRequests`' eviction key, avoiding a
    /// `merged_blocks` lookup at queue time.
    pub base_block_hash: B256,
    pub slot: u64,
    pub parent_beacon_block_root: B256,
    pub proposer_fee_recipient: Address,
    pub registered_gas_limit: u64,
    pub apply_blacklist: bool,
    pub inclusion_list: InclusionListWithMetadata,
    pub receive_ns: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockMergeResponse {
    pub base_block_hash: B256,
    pub execution_payload: ExecutionPayload,
    pub execution_requests: ExecutionRequests,
    /// The merged block's full blob set (base block's own blob txs plus any newly
    /// appended ones), re-attached from `BlockMergingTile`'s own cache of blobs seen in
    /// submissions this slot.
    pub blobs_bundle: BlobsBundle,
    /// Total value for the proposer
    pub proposer_value: U256,
    pub base_builder_revenue: U256,
    pub relay_revenue: U256,
    pub builder_inclusions: HashMap<Address, BuilderInclusionResult>,
    /// Index, within `execution_payload.transactions`, of the base block's own proposer
    /// payment tx. Base txs keep their original positions in a merged block -- only new
    /// content is ever appended after them -- so this is always
    /// `execution_payload.transactions.len() - <appended order txs> - 2` (the `- 2` for the
    /// appended order txs' own count and the trailing distribution tx). Lets a merged-block
    /// validator recognise the base block's payment directly instead of scanning every tx.
    pub base_payment_tx_index: usize,
    pub trace: MergedBlockTrace,
}

/// Large payload stored in `SharedVector` for auctioneer → sim tile transfer.
pub enum SimRequest {
    Validate { req: Box<ValidationRequest>, fast_track: bool },
    ValidateMerged(Box<MergedValidationRequest>),
}

/// Large payload stored in `SharedVector` for sim tile → auctioneer transfer.
// Stored inline in `SharedVector`; boxing would add a heap alloc on the hot path.
#[allow(clippy::large_enum_variant)]
pub enum SimResult {
    Validate(ValidationResult),
    ValidateMerged(MergedSimulationResult),
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
