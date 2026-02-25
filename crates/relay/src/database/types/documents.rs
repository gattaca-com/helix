use alloy_primitives::U256;
use helix_common::{
    BuilderConfig,
    api::data_api::{
        DeliveredPayloadsResponse, DeliveredPayloadsResponseV2, ReceivedBlocksResponse,
        ReceivedBlocksResponseV2,
    },
};
use helix_types::BidTrace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct DeliveredPayloadDocument {
    pub bid_trace: BidTrace,
    pub block_number: u64,
    pub num_txs: usize,
    pub region: Option<String>,
}

impl DeliveredPayloadDocument {
    pub fn value(&self) -> U256 {
        self.bid_trace.value
    }
}

impl From<DeliveredPayloadDocument> for DeliveredPayloadsResponse {
    fn from(doc: DeliveredPayloadDocument) -> Self {
        Self {
            slot: doc.bid_trace.slot(),
            parent_hash: doc.bid_trace.parent_hash,
            block_hash: doc.bid_trace.block_hash,
            builder_pubkey: doc.bid_trace.builder_pubkey,
            proposer_pubkey: doc.bid_trace.proposer_pubkey,
            proposer_fee_recipient: doc.bid_trace.proposer_fee_recipient,
            value: doc.bid_trace.value,
            gas_limit: doc.bid_trace.gas_limit,
            gas_used: doc.bid_trace.gas_used,
            block_number: doc.block_number,
            num_tx: doc.num_txs as u64,
        }
    }
}

impl From<DeliveredPayloadDocument> for DeliveredPayloadsResponseV2 {
    fn from(doc: DeliveredPayloadDocument) -> Self {
        Self {
            slot: doc.bid_trace.slot(),
            parent_hash: doc.bid_trace.parent_hash,
            block_hash: doc.bid_trace.block_hash,
            builder_pubkey: doc.bid_trace.builder_pubkey,
            proposer_pubkey: doc.bid_trace.proposer_pubkey,
            proposer_fee_recipient: doc.bid_trace.proposer_fee_recipient,
            value: doc.bid_trace.value,
            gas_limit: doc.bid_trace.gas_limit,
            gas_used: doc.bid_trace.gas_used,
            block_number: doc.block_number,
            num_tx: doc.num_txs as u64,
            region: doc.region,
        }
    }
}

pub type BuilderInfoDocument = BuilderConfig;

#[derive(Serialize, Deserialize, Debug)]
pub struct BidSubmissionDocument {
    pub timestamp: u64,
    pub bid_trace: BidTrace,
    pub block_number: u64,
    pub num_txs: usize,
    pub region: Option<String>,
}

impl From<BidSubmissionDocument> for ReceivedBlocksResponse {
    fn from(value: BidSubmissionDocument) -> Self {
        ReceivedBlocksResponse {
            slot: value.bid_trace.slot(),
            parent_hash: value.bid_trace.parent_hash,
            block_hash: value.bid_trace.block_hash,
            builder_pubkey: value.bid_trace.builder_pubkey,
            proposer_pubkey: value.bid_trace.proposer_pubkey,
            proposer_fee_recipient: value.bid_trace.proposer_fee_recipient,
            gas_limit: value.bid_trace.gas_limit,
            gas_used: value.bid_trace.gas_used,
            value: value.bid_trace.value,
            block_number: value.block_number,
            num_tx: value.num_txs as u64,
            // Other mev-boost relays return this timestamp in seconds
            timestamp: value.timestamp / 1_000_000_000,
            timestamp_ms: value.timestamp / 1_000_000,
        }
    }
}

impl From<BidSubmissionDocument> for ReceivedBlocksResponseV2 {
    fn from(value: BidSubmissionDocument) -> Self {
        ReceivedBlocksResponseV2 {
            slot: value.bid_trace.slot(),
            parent_hash: value.bid_trace.parent_hash,
            block_hash: value.bid_trace.block_hash,
            builder_pubkey: value.bid_trace.builder_pubkey,
            proposer_pubkey: value.bid_trace.proposer_pubkey,
            proposer_fee_recipient: value.bid_trace.proposer_fee_recipient,
            gas_limit: value.bid_trace.gas_limit,
            gas_used: value.bid_trace.gas_used,
            value: value.bid_trace.value,
            block_number: value.block_number,
            num_tx: value.num_txs as u64,
            // Other mev-boost relays return this timestamp in seconds
            timestamp: value.timestamp / 1_000_000_000,
            timestamp_ms: value.timestamp / 1_000_000,
            region: value.region,
        }
    }
}
