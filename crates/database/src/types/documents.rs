use std::sync::Arc;

use ethereum_consensus::{
    clock::get_current_unix_time_in_nanos,
    primitives::{BlsPublicKey, Hash32},
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
};
use serde::{Deserialize, Serialize};

use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry,
        data_api::{DeliveredPayloadsResponse, ReceivedBlocksResponse},
    },
    bid_submission::{BidSubmission, BidTrace, SignedBidSubmission},
    builder_info::BuilderInfo,
    simulator::BlockSimError,
    GetPayloadTrace, SubmissionTrace,
};

#[derive(Serialize, Deserialize)]
pub struct ProposerDutiesDocument {
    pub duties: Vec<BuilderGetValidatorsResponseEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TooLateGetPayloadDocument {
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKey,
    pub payload_hash: Hash32,
    pub message_received: u64,
    pub payload_fetched: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeliveredPayloadDocument {
    pub bid_trace: BidTrace,
    pub payload: Arc<ExecutionPayload>,
    pub latency_trace: GetPayloadTrace,
}

impl DeliveredPayloadDocument {
    pub fn value(&self) -> U256 {
        self.bid_trace.value
    }
}

impl From<DeliveredPayloadDocument> for DeliveredPayloadsResponse {
    fn from(doc: DeliveredPayloadDocument) -> Self {
        Self {
            slot: doc.bid_trace.slot,
            parent_hash: doc.bid_trace.parent_hash,
            block_hash: doc.bid_trace.block_hash,
            builder_pubkey: doc.bid_trace.builder_public_key,
            proposer_pubkey: doc.bid_trace.proposer_public_key,
            proposer_fee_recipient: doc.bid_trace.proposer_fee_recipient,
            value: doc.bid_trace.value,
            gas_limit: doc.bid_trace.gas_limit,
            gas_used: doc.bid_trace.gas_used,
            block_number: doc.payload.block_number(),
            num_tx: doc.payload.transactions().len(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubmissionTraceDocument {
    pub block_hash: Hash32,
    pub trace: SubmissionTrace,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DemotionDocument {
    pub pub_key: BlsPublicKey,
    pub demotion_time: u64,
}

impl DemotionDocument {
    pub fn new(pub_key: BlsPublicKey, demotion_time: u64) -> Self {
        Self { pub_key, demotion_time }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BuilderInfoDocument {
    pub pub_key: BlsPublicKey,
    pub builder_info: BuilderInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockSimErrorDocument {
    pub block_hash: ByteVector<32>,
    pub sim_error: BlockSimError,
}

impl BlockSimErrorDocument {
    pub fn new(block_hash: ByteVector<32>, sim_error: BlockSimError) -> Self {
        Self { block_hash, sim_error }
    }
}

#[derive(Serialize, Deserialize)]
pub struct KnownValidatorsDocument {
    pub public_key: BlsPublicKey,
    pub index: usize,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct BidSubmissionDocument {
    pub timestamp: u64,
    pub bid_trace: BidTrace,
    pub block_number: u64,
    pub num_txs: usize,
}

impl BidSubmissionDocument {
    pub fn from_signed_submission(value: &SignedBidSubmission) -> Self {
        Self {
            timestamp: get_current_unix_time_in_nanos() as u64,
            bid_trace: value.bid_trace().clone(),
            block_number: value.block_number(),
            num_txs: value.transactions().len(),
        }
    }
}

impl From<BidSubmissionDocument> for ReceivedBlocksResponse {
    fn from(value: BidSubmissionDocument) -> Self {
        ReceivedBlocksResponse {
            slot: value.bid_trace.slot,
            parent_hash: value.bid_trace.parent_hash,
            block_hash: value.bid_trace.block_hash,
            builder_pubkey: value.bid_trace.builder_public_key,
            proposer_pubkey: value.bid_trace.proposer_public_key,
            proposer_fee_recipient: value.bid_trace.proposer_fee_recipient,
            gas_limit: value.bid_trace.gas_limit,
            gas_used: value.bid_trace.gas_used,
            value: value.bid_trace.value,
            block_number: value.block_number,
            num_tx: value.num_txs,
            timestamp: value.timestamp,
            timestamp_ms: value.timestamp / 1_000_000,
        }
    }
}
