use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    SubmissionTrace, ValidatorPreferences, api::builder_api::InclusionListWithMetadata,
    bid_submission::OptimisticVersion, simulator::BlockSimError,
};
use helix_types::{
    BidTrace, BlobsBundle, BlsPublicKeyBytes, BlsSignatureBytes, BuilderInclusionResult,
    ExecutionPayload, ExecutionRequests, MergeableOrderWithOrigin, MergedBlockTrace,
    SignedBidSubmission, SubmissionVersion,
};
use tokio::sync::oneshot;

use crate::{SlotData, SubmissionPayload, auctioneer::types::SubmissionResult};

pub mod client;
pub mod manager;

// TODO: refactor this in a SignedBidSubmission + extra fields
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSimRequest {
    #[serde(with = "serde_utils::quoted_u64")]
    pub registered_gas_limit: u64,
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignatureBytes,
    pub proposer_preferences: ValidatorPreferences,
    pub blobs_bundle: Option<Arc<BlobsBundle>>,
    pub execution_requests: Option<Arc<ExecutionRequests>>,
    pub parent_beacon_block_root: Option<B256>,
    pub inclusion_list: Option<InclusionListWithMetadata>,
    pub apply_blacklist: bool,
}

impl BlockSimRequest {
    pub fn new(
        registered_gas_limit: u64,
        block: &SignedBidSubmission,
        proposer_preferences: ValidatorPreferences,
        parent_beacon_block_root: Option<B256>,
        inclusion_list: Option<InclusionListWithMetadata>,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace().clone(),
            execution_payload: block.execution_payload_ref().clone(),
            signature: *block.signature(),
            apply_blacklist: proposer_preferences.filtering.is_regional(),
            proposer_preferences,
            blobs_bundle: Some(block.blobs_bundle().clone()),
            execution_requests: Some(block.execution_requests_ref().clone()),
            parent_beacon_block_root,
            inclusion_list,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockMergeRequestRef<'a> {
    /// The original payload value
    pub original_value: U256,
    pub proposer_fee_recipient: Address,
    pub execution_payload: &'a ExecutionPayload,
    pub parent_beacon_block_root: Option<B256>,
    pub merging_data: &'a [MergeableOrderWithOrigin],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockMergeRequest {
    pub bid_slot: u64,
    /// The serialized request
    pub request: serde_json::Value,
    /// The block hash of the execution payload
    pub block_hash: B256,
    /// The trace of the merged block
    pub trace: MergedBlockTrace,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub message: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct BlockSimRpcResponse {
    pub error: Option<JsonRpcError>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(untagged)]
pub enum RpcResult<T> {
    Ok { result: T },
    Err { error: JsonRpcError },
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

pub struct SimulatorRequest {
    pub request: BlockSimRequest,
    pub is_top_bid: bool,
    pub version: SubmissionVersion,
    pub submission: SignedBidSubmission,
    /// None if optimistic
    pub res_tx: Option<oneshot::Sender<SubmissionResult>>,
    pub trace: SubmissionTrace,
    // only Some for dehydrated submissions
    pub tx_root: Option<B256>,
}

impl SimulatorRequest {
    pub fn new(bid: &SubmissionPayload, slot_data: &SlotData) -> Self {
        let request = BlockSimRequest::new(
            slot_data.registration_data.entry.registration.message.gas_limit,
            &bid.signed_bid_submission,
            slot_data.registration_data.entry.preferences.clone(),
            bid.parent_beacon_block_root,
            slot_data.il.clone(),
        );

        Self {
            request,
            is_top_bid: true,
            res_tx: None,
            submission: bid.signed_bid_submission.clone(),
            trace: bid.submission_trace.clone(),
            tx_root: bid.tx_root,
            version: bid.submission_version,
        }
    }

    pub fn on_receive_ns(&self) -> u64 {
        self.trace.receive
    }

    // TODO: use a "score" eg how close to top bid even if below
    pub fn sort_key(&self) -> (u8, u8, u64) {
        let open = if self.is_closed() { 0 } else { 1 };
        let top = if self.is_top_bid { 1 } else { 0 };
        (open, top, u64::MAX - self.on_receive_ns())
    }

    pub fn is_closed(&self) -> bool {
        self.res_tx.as_ref().is_some_and(|r| r.is_closed())
    }

    pub fn bid_slot(&self) -> u64 {
        self.request.message.slot
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        &self.request.message.builder_pubkey
    }

    pub fn optimistic_version(&self) -> OptimisticVersion {
        if self.res_tx.is_some() { OptimisticVersion::NotOptimistic } else { OptimisticVersion::V1 }
    }
}
