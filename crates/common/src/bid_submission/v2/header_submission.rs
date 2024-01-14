use ethereum_consensus::{
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    ssz::prelude::*, signing::verify_signature,
    types::mainnet::ExecutionPayloadHeader,
    deneb::mainnet::{BYTES_PER_LOGS_BLOOM, MAX_EXTRA_DATA_BYTES},
    capella::Withdrawal,
    altair::Bytes32,
    Fork,
};
use helix_utils::signing::compute_builder_signing_root;
use crate::{bid_submission::{BidTrace, BidSubmission}, versioned_payload_header::VersionedExecutionPayloadHeader, deneb::BlobsBundle};


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmission {
    pub bid_trace: BidTrace,
    #[serde(flatten)]
    pub versioned_execution_payload: VersionedExecutionPayloadHeader,
}

impl Default for HeaderSubmission {
    fn default() -> Self {
        Self {
            bid_trace: BidTrace::default(),
            versioned_execution_payload: VersionedExecutionPayloadHeader::default(),
        }
    }
}

#[derive(Clone, Debug, Default, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedHeaderSubmission {
    pub message: HeaderSubmission,
    pub signature: BlsSignature,
}

impl BidSubmission for SignedHeaderSubmission {
    fn bid_trace(&self) -> &BidTrace {
        &self.message.bid_trace
    }

    fn signature(&self) -> &BlsSignature {
        &self.signature
    }

    fn slot(&self) -> Slot {
        self.message.bid_trace.slot
    }

    fn parent_hash(&self) -> &Hash32 {
        &self.message.bid_trace.parent_hash
    }

    fn block_hash(&self) -> &Hash32 {
        &self.message.bid_trace.block_hash
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        &self.message.bid_trace.builder_public_key
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.message.bid_trace.proposer_public_key
    }

    fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        &self.message.bid_trace.proposer_fee_recipient
    }

    fn gas_limit(&self) -> u64 {
        self.message.bid_trace.gas_limit
    }

    fn gas_used(&self) -> u64 {
        self.message.bid_trace.gas_used
    }

    fn value(&self) -> U256 {
        self.message.bid_trace.value
    }

    fn fee_recipient(&self) -> &ExecutionAddress {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.fee_recipient,
            ExecutionPayloadHeader::Capella(payload) => &payload.fee_recipient,
            ExecutionPayloadHeader::Deneb(payload) => &payload.fee_recipient,
        }
    }

    fn state_root(&self) -> &Bytes32 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.state_root,
            ExecutionPayloadHeader::Capella(payload) => &payload.state_root,
            ExecutionPayloadHeader::Deneb(payload) => &payload.state_root,
        }
    }

    fn receipts_root(&self) -> &Bytes32 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.receipts_root,
            ExecutionPayloadHeader::Capella(payload) => &payload.receipts_root,
            ExecutionPayloadHeader::Deneb(payload) => &payload.receipts_root,
        }
    }

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.logs_bloom,
            ExecutionPayloadHeader::Capella(payload) => &payload.logs_bloom,
            ExecutionPayloadHeader::Deneb(payload) => &payload.logs_bloom,
        }
    }

    fn prev_randao(&self) -> &Bytes32 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.prev_randao,
            ExecutionPayloadHeader::Capella(payload) => &payload.prev_randao,
            ExecutionPayloadHeader::Deneb(payload) => &payload.prev_randao,
        }
    }

    fn block_number(&self) -> u64 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => payload.block_number,
            ExecutionPayloadHeader::Capella(payload) => payload.block_number,
            ExecutionPayloadHeader::Deneb(payload) => payload.block_number,
        }
    }

    fn timestamp(&self) -> u64 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => payload.timestamp,
            ExecutionPayloadHeader::Capella(payload) => payload.timestamp,
            ExecutionPayloadHeader::Deneb(payload) => payload.timestamp,
        }
    }

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.extra_data,
            ExecutionPayloadHeader::Capella(payload) => &payload.extra_data,
            ExecutionPayloadHeader::Deneb(payload) => &payload.extra_data,
        }
    }

    fn base_fee_per_gas(&self) -> &U256 {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(payload) => &payload.base_fee_per_gas,
            ExecutionPayloadHeader::Capella(payload) => &payload.base_fee_per_gas,
            ExecutionPayloadHeader::Deneb(payload) => &payload.base_fee_per_gas,
        }
    }

    fn withdrawals(&self) -> Option<&[Withdrawal]> {
        None
    }

    fn consensus_version(&self) -> Fork {
        match &self.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(_) => Fork::Bellatrix,
            ExecutionPayloadHeader::Capella(_) => Fork::Capella,
            ExecutionPayloadHeader::Deneb(_) => Fork::Deneb,
        }
    }

    fn is_full_payload(&self) -> bool {
        false
    }
}

impl SignedHeaderSubmission {
    pub fn verify_signature(&mut self, context: &ethereum_consensus::state_transition::Context) -> Result<(), ethereum_consensus::Error> {
        let signing_root = compute_builder_signing_root(&mut self.message.bid_trace, context)?;
        let public_key = &self.message.bid_trace.builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), &self.signature)
    }

    pub fn execution_payload_header(&self) -> &ExecutionPayloadHeader {
        &self.message.versioned_execution_payload.execution_payload_header
    }

    pub fn blobs_bundle(&self) -> Option<&BlobsBundle> {
        self.message.versioned_execution_payload.blobs_bundle.as_ref()
    }
}
