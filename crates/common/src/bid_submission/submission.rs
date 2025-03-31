use ethereum_consensus::{
    altair::Bytes32,
    capella::Withdrawal,
    deneb::mainnet::{
        BYTES_PER_LOGS_BLOOM, MAX_BYTES_PER_TRANSACTION, MAX_EXTRA_DATA_BYTES,
        MAX_TRANSACTIONS_PER_PAYLOAD,
    },
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
    Fork,
};
use helix_utils::signing::verify_signed_builder_message;

use crate::{
    bid_submission::{BidSubmission, BidTrace},
    capella,
    deneb::BlobsBundle,
    electra::ExecutionRequests,
    proofs::InclusionProofs,
    versioned_payload::PayloadAndBlobs,
};

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
#[ssz(transparent)]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Capella(SignedBidSubmissionCapella),
    Deneb(SignedBidSubmissionDeneb),
    DenebWithProofs(SignedBidSubmissionDenebWithProofs),
    Electra(SignedBidSubmissionElectra),
}

impl BidSubmission for SignedBidSubmission {
    fn proofs(&self) -> Option<&InclusionProofs> {
        match self {
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                Some(&signed_bid_submission.proofs)
            }
            _ => None,
        }
    }

    fn bid_trace(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message
            }
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    fn signature(&self) -> &BlsSignature {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.signature,
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.signature,
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.signature
            }
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.signature,
        }
    }

    fn slot(&self) -> Slot {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.slot
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => signed_bid_submission.message.slot,
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.message.slot
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.slot
            }
        }
    }

    fn parent_hash(&self) -> &Hash32 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
        }
    }

    fn block_hash(&self) -> &Hash32 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
        }
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
        }
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
        }
    }

    fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
        }
    }

    fn gas_used(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
        }
    }

    fn value(&self) -> U256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
        }
    }

    fn fee_recipient(&self) -> &ExecutionAddress {
        self.execution_payload().fee_recipient()
    }

    fn state_root(&self) -> &Bytes32 {
        self.execution_payload().state_root()
    }

    fn receipts_root(&self) -> &Bytes32 {
        self.execution_payload().receipts_root()
    }

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        self.execution_payload().logs_bloom()
    }

    fn prev_randao(&self) -> &Bytes32 {
        self.execution_payload().prev_randao()
    }

    fn block_number(&self) -> u64 {
        self.execution_payload().block_number()
    }

    fn timestamp(&self) -> u64 {
        self.execution_payload().timestamp()
    }

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        self.execution_payload().extra_data()
    }

    fn base_fee_per_gas(&self) -> U256 {
        *self.execution_payload().base_fee_per_gas()
    }

    fn withdrawals(&self) -> Option<&[Withdrawal]> {
        match &self.execution_payload() {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => Some(&payload.withdrawals),
            ExecutionPayload::Deneb(payload) => Some(&payload.withdrawals),
            ExecutionPayload::Electra(payload) => Some(&payload.withdrawals),
        }
    }

    fn withdrawals_root(&self) -> Option<Node> {
        match &self.execution_payload() {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => {
                let withdrawals = payload.withdrawals.clone();
                match withdrawals.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Deneb(payload) => {
                let withdrawals = payload.withdrawals.clone();
                match withdrawals.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Electra(payload) => {
                let withdrawals = payload.withdrawals.clone();
                match withdrawals.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
        }
    }

    fn transactions_root(&self) -> Option<Node> {
        match &self.execution_payload() {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => {
                let transactions = payload.transactions.clone();
                match transactions.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Deneb(payload) => {
                let transactions = payload.transactions.clone();
                match transactions.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Electra(payload) => {
                let transactions = payload.transactions.clone();
                match transactions.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
        }
    }

    fn consensus_version(&self) -> Fork {
        match self {
            SignedBidSubmission::Electra(_) => Fork::Electra,
            SignedBidSubmission::Deneb(_) => Fork::Deneb,
            SignedBidSubmission::DenebWithProofs(_) => Fork::Deneb,
            SignedBidSubmission::Capella(_) => Fork::Capella,
        }
    }

    fn is_full_payload(&self) -> bool {
        true
    }
}

impl SignedBidSubmission {
    pub fn verify_signature(
        &mut self,
        context: &ethereum_consensus::state_transition::Context,
    ) -> Result<(), ethereum_consensus::Error> {
        let public_key = self.builder_public_key().clone();
        let signature = self.signature().clone();
        verify_signed_builder_message(self.message_mut(), &signature, &public_key, context)
    }

    pub fn transactions(
        &self,
    ) -> &List<ByteList<MAX_BYTES_PER_TRANSACTION>, MAX_TRANSACTIONS_PER_PAYLOAD> {
        match &self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
        }
    }

    pub fn blobs_bundle(&self) -> Option<&BlobsBundle> {
        match &self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(&signed_bid_submission.blobs_bundle)
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                Some(&signed_bid_submission.blobs_bundle)
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                Some(&signed_bid_submission.blobs_bundle)
            }
            SignedBidSubmission::Capella(_) => None,
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.message
            }
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => &mut signed_bid_submission.message,
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
        }
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
        }
    }

    pub fn execution_payload_mut(&mut self) -> &mut ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
        }
    }

    pub fn execution_payload_moved(self) -> ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
            SignedBidSubmission::DenebWithProofs(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        match self {
            SignedBidSubmission::Electra(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: self.blobs_bundle().cloned(),
            },
            SignedBidSubmission::Deneb(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: self.blobs_bundle().cloned(),
            },
            SignedBidSubmission::DenebWithProofs(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: self.blobs_bundle().cloned(),
            },
            SignedBidSubmission::Capella(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: None,
            },
        }
    }

    pub fn execution_requests(&self) -> Option<&ExecutionRequests> {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(&signed_bid_submission.execution_requests)
            }
            SignedBidSubmission::Deneb(_) => None,
            SignedBidSubmission::DenebWithProofs(_) => None,
            SignedBidSubmission::Capella(_) => None,
        }
    }
}

impl Default for SignedBidSubmission {
    fn default() -> Self {
        Self::Capella(SignedBidSubmissionCapella {
            message: BidTrace::default(),
            execution_payload: ExecutionPayload::Capella(capella::ExecutionPayload::default()),
            signature: BlsSignature::default(),
        })
    }
}

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionCapella {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionDeneb {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionDenebWithProofs {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
    /// The Merkle proofs of inclusion as needed by the Constraints API.
    /// Reference: <https://docs.boltprotocol.xyz/technical-docs/api/builder#get_header_with_proofs>
    pub proofs: InclusionProofs,
}

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundle,
    pub execution_requests: ExecutionRequests,
    pub signature: BlsSignature,
}
