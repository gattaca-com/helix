use std::{collections::HashMap, sync::Arc, time::Instant};

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    api::builder_api::InclusionListWithMetadata, bid_submission::BidSubmission,
    simulator::BlockSimError, ValidatorPreferences,
};
use helix_types::{
    BidTrace, BlobsBundle, BlockMergingPreferences, BlsPublicKeyBytes, BlsSignatureBytes,
    BuilderInclusionResult, ExecutionPayload, ExecutionRequests, MergeableOrderWithOrigin,
    SignedBidSubmission,
};
use serde_json::json;
use tokio::sync::oneshot;

use crate::auctioneer::types::SubmissionResult;

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
            execution_requests: block.execution_requests(),
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

type MergeResult = Result<BlockMergeResponse, BlockSimError>;

pub struct BlockMergeRequest {
    /// The serialized request
    pub request: serde_json::Value,
    /// The block hash of the execution payload
    pub block_hash: B256,
    pub res_tx: oneshot::Sender<MergeResult>,
}

impl BlockMergeRequest {
    pub fn new(
        original_value: U256,
        proposer_fee_recipient: Address,
        execution_payload: &ExecutionPayload,
        parent_beacon_block_root: Option<B256>,
        merging_data: &[MergeableOrderWithOrigin],
    ) -> (Self, oneshot::Receiver<MergeResult>) {
        let block_hash = execution_payload.block_hash;
        // We serialize the request ahead of time, to avoid copying the original
        // payload and merging data.
        let request_ref = BlockMergeRequestRef {
            original_value,
            proposer_fee_recipient,
            execution_payload,
            parent_beacon_block_root,
            merging_data,
        };
        let request = json!(request_ref);
        let (tx, rx) = oneshot::channel();
        (Self { request, block_hash, res_tx: tx }, rx)
    }
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

impl<T> RpcResult<T> {
    pub fn as_error(&self) -> Option<&JsonRpcError> {
        match self {
            RpcResult::Err { error } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockMergeResponse {
    pub execution_payload: ExecutionPayload,
    pub execution_requests: ExecutionRequests,
    /// Versioned hashes of the appended blob transactions.
    pub appended_blobs: Vec<B256>,
    /// Total value for the proposer
    pub proposer_value: U256,
    pub builder_inclusions: HashMap<Address, BuilderInclusionResult>,
}

pub type SimResult = Result<(), BlockSimError>;
// (simulator id, paused_until)
pub type SimReponse = (usize, Option<Instant>);

pub struct SimulatorRequest {
    pub request: BlockSimRequest,
    /// when submission was received in ns
    pub on_receive_ns: u64,
    pub is_top_bid: bool,
    pub is_optimistic: bool,
    pub submission: SignedBidSubmission,
    pub res_tx: Option<oneshot::Sender<SubmissionResult>>,
    pub merging_preferences: BlockMergingPreferences,
}

impl SimulatorRequest {
    // TODO: use a "score" eg how close to top bid even if below
    pub fn sort_key(&self) -> (u8, u8, u64) {
        let open = if self.is_closed() { 0 } else { 1 };
        let top = if self.is_top_bid { 1 } else { 0 };
        (open, top, u64::MAX - self.on_receive_ns)
    }

    pub fn is_closed(&self) -> bool {
        !self.is_optimistic && self.res_tx.as_ref().is_some_and(|r| r.is_closed())
    }

    pub fn bid_slot(&self) -> u64 {
        self.request.message.slot
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        &self.request.message.builder_pubkey
    }
}
