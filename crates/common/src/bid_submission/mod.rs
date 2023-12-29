use ethereum_consensus::{
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    serde::as_str, ssz::prelude::*, signing::verify_signature,
    types::mainnet::ExecutionPayload,
    deneb::mainnet::{BYTES_PER_LOGS_BLOOM, MAX_EXTRA_DATA_BYTES, MAX_BYTES_PER_TRANSACTION, MAX_TRANSACTIONS_PER_PAYLOAD},
    capella::Withdrawal,
    altair::Bytes32,
    Fork,
};
use helix_utils::signing::compute_builder_signing_root;
use crate::capella;


#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BidTrace {
    #[serde(with = "as_str")]
    pub slot: Slot,
    pub parent_hash: Hash32,
    pub block_hash: Hash32,
    #[serde(rename = "builder_pubkey")]
    pub builder_public_key: BlsPublicKey,
    #[serde(rename = "proposer_pubkey")]
    pub proposer_public_key: BlsPublicKey,
    pub proposer_fee_recipient: ExecutionAddress,
    #[serde(with = "as_str")]
    pub gas_limit: u64,
    #[serde(with = "as_str")]
    pub gas_used: u64,
    #[serde(with = "as_str")]
    pub value: U256,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBidSubmission {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
}

impl SignedBidSubmission {
    pub fn message(&self) -> &BidTrace {
        &self.message
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        &self.execution_payload
    }

    pub fn signature(&self) -> &BlsSignature {
        &self.signature
    }

    pub fn slot(&self) -> Slot {
        self.message.slot
    }

    pub fn parent_hash(&self) -> &Hash32 {
        &self.message.parent_hash
    }

    pub fn block_hash(&self) -> &Hash32 {
        &self.message.block_hash
    }

    pub fn builder_public_key(&self) -> &BlsPublicKey {
        &self.message.builder_public_key
    }

    pub fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.message.proposer_public_key
    }

    pub fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        &self.message.proposer_fee_recipient
    }

    pub fn gas_limit(&self) -> u64 {
        self.message.gas_limit
    }

    pub fn gas_used(&self) -> u64 {
        self.message.gas_used
    }

    pub fn value(&self) -> U256 {
        self.message.value
    }

    pub fn verify_signature(&mut self, context: &ethereum_consensus::state_transition::Context) -> Result<(), ethereum_consensus::Error> {
        let signing_root = compute_builder_signing_root(&mut self.message, context)?;
        let public_key = &self.message.builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), &self.signature)
    }

    pub fn fee_recipient(&self) -> &ExecutionAddress {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.fee_recipient,
            ExecutionPayload::Capella(payload) => &payload.fee_recipient,
            ExecutionPayload::Deneb(payload) => &payload.fee_recipient,
        }
    }

    pub fn state_root(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.state_root,
            ExecutionPayload::Capella(payload) => &payload.state_root,
            ExecutionPayload::Deneb(payload) => &payload.state_root,
        }
    }

    pub fn receipts_root(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.receipts_root,
            ExecutionPayload::Capella(payload) => &payload.receipts_root,
            ExecutionPayload::Deneb(payload) => &payload.receipts_root,
        }
    }

    pub fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.logs_bloom,
            ExecutionPayload::Capella(payload) => &payload.logs_bloom,
            ExecutionPayload::Deneb(payload) => &payload.logs_bloom,
        }
    }

    pub fn prev_randao(&self) -> &Bytes32 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.prev_randao,
            ExecutionPayload::Capella(payload) => &payload.prev_randao,
            ExecutionPayload::Deneb(payload) => &payload.prev_randao,
        }
    }

    pub fn block_number(&self) -> u64 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => payload.block_number,
            ExecutionPayload::Capella(payload) => payload.block_number,
            ExecutionPayload::Deneb(payload) => payload.block_number,
        }
    }

    pub fn timestamp(&self) -> u64 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => payload.timestamp,
            ExecutionPayload::Capella(payload) => payload.timestamp,
            ExecutionPayload::Deneb(payload) => payload.timestamp,
        }
    }

    pub fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.extra_data,
            ExecutionPayload::Capella(payload) => &payload.extra_data,
            ExecutionPayload::Deneb(payload) => &payload.extra_data,
        }
    }

    pub fn base_fee_per_gas(&self) -> &U256 {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(payload) => &payload.base_fee_per_gas,
            ExecutionPayload::Capella(payload) => &payload.base_fee_per_gas,
            ExecutionPayload::Deneb(payload) => &payload.base_fee_per_gas,
        }
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

    pub fn withdrawals(&self) -> Option<&[Withdrawal]> {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => Some(&payload.withdrawals),
            ExecutionPayload::Deneb(payload) => Some(&payload.withdrawals),
        }
    }

    pub fn consensus_version(&self) -> Fork {
        match &self.execution_payload {
            ExecutionPayload::Bellatrix(_) => Fork::Bellatrix,
            ExecutionPayload::Capella(_) => Fork::Capella,
            ExecutionPayload::Deneb(_) => Fork::Deneb,
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
