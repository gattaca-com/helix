use ethereum_consensus::{
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    ssz::prelude::*, signing::verify_signature,
    types::mainnet::ExecutionPayload,
    deneb::mainnet::{BYTES_PER_LOGS_BLOOM, MAX_EXTRA_DATA_BYTES, MAX_BYTES_PER_TRANSACTION, MAX_TRANSACTIONS_PER_PAYLOAD},
    capella::Withdrawal,
    altair::Bytes32,
    Fork,
};
use helix_utils::signing::compute_builder_signing_root;
use crate::{capella, bid_submission::{BidTrace, BidSubmission}};


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBidSubmission {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
}

impl BidSubmission for SignedBidSubmission {
    fn bid_trace(&self) -> &BidTrace {
        &self.message
    }

    fn signature(&self) -> &BlsSignature {
        &self.signature
    }

    fn slot(&self) -> Slot {
        self.message.slot
    }

    fn parent_hash(&self) -> &Hash32 {
        &self.message.parent_hash
    }

    fn block_hash(&self) -> &Hash32 {
        &self.message.block_hash
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        &self.message.builder_public_key
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.message.proposer_public_key
    }

    fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        &self.message.proposer_fee_recipient
    }

    fn gas_limit(&self) -> u64 {
        self.message.gas_limit
    }

    fn gas_used(&self) -> u64 {
        self.message.gas_used
    }

    fn value(&self) -> U256 {
        self.message.value
    }

    fn fee_recipient(&self) -> &ExecutionAddress {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.fee_recipient,
            ExecutionPayload::Capella(payload) => &payload.fee_recipient,
            ExecutionPayload::Deneb(payload) => &payload.fee_recipient,
        }
    }

    fn state_root(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.state_root,
            ExecutionPayload::Capella(payload) => &payload.state_root,
            ExecutionPayload::Deneb(payload) => &payload.state_root,
        }
    }

    fn receipts_root(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.receipts_root,
            ExecutionPayload::Capella(payload) => &payload.receipts_root,
            ExecutionPayload::Deneb(payload) => &payload.receipts_root,
        }
    }

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.logs_bloom,
            ExecutionPayload::Capella(payload) => &payload.logs_bloom,
            ExecutionPayload::Deneb(payload) => &payload.logs_bloom,
        }
    }

    fn prev_randao(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.prev_randao,
            ExecutionPayload::Capella(payload) => &payload.prev_randao,
            ExecutionPayload::Deneb(payload) => &payload.prev_randao,
        }
    }

    fn block_number(&self) -> u64 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => payload.block_number,
            ExecutionPayload::Capella(payload) => payload.block_number,
            ExecutionPayload::Deneb(payload) => payload.block_number,
        }
    }

    fn timestamp(&self) -> u64 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => payload.timestamp,
            ExecutionPayload::Capella(payload) => payload.timestamp,
            ExecutionPayload::Deneb(payload) => payload.timestamp,
        }
    }

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.extra_data,
            ExecutionPayload::Capella(payload) => &payload.extra_data,
            ExecutionPayload::Deneb(payload) => &payload.extra_data,
        }
    }

    fn base_fee_per_gas(&self) -> &U256 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.base_fee_per_gas,
            ExecutionPayload::Capella(payload) => &payload.base_fee_per_gas,
            ExecutionPayload::Deneb(payload) => &payload.base_fee_per_gas,
        }
    }

    fn withdrawals(&self) -> Option<&[Withdrawal]> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => Some(&payload.withdrawals),
            ExecutionPayload::Deneb(payload) => Some(&payload.withdrawals),
        }
    }

    fn consensus_version(&self) -> Fork {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(_) => Fork::Bellatrix,
            ExecutionPayload::Capella(_) => Fork::Capella,
            ExecutionPayload::Deneb(_) => Fork::Deneb,
        }
    }

    fn is_full_payload(&self) -> bool {
        true
    }
}

impl SignedBidSubmission {
    pub fn verify_signature(&mut self, context: &ethereum_consensus::state_transition::Context) -> Result<(), ethereum_consensus::Error> {
        let signing_root = compute_builder_signing_root(&mut self.message, context)?;
        let public_key = &self.message.builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), &self.signature)
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        &self.execution_payload
    }

    pub fn transactions(
        &self,
    ) -> &List<ByteList<MAX_BYTES_PER_TRANSACTION>, MAX_TRANSACTIONS_PER_PAYLOAD> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.transactions,
            ExecutionPayload::Capella(payload) => &payload.transactions,
            ExecutionPayload::Deneb(payload) => &payload.transactions,
        }
    }
}

impl Default for SignedBidSubmission {
    fn default() -> Self {
        Self {
            message: BidTrace::default(),
            execution_payload: ExecutionPayload::Capella(capella::ExecutionPayload::default()),
            signature: BlsSignature::default(),
        }
    }
}
