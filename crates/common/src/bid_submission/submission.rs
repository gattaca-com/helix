use crate::{
    bid_submission::{BidSubmission, BidTrace},
    capella,
    deneb::BlobsBundle,
    proofs::InclusionProofs,
    versioned_payload::PayloadAndBlobs,
};
use ethereum_consensus::{
    altair::Bytes32,
    capella::Withdrawal,
    deneb::mainnet::{
        BYTES_PER_LOGS_BLOOM, MAX_BYTES_PER_TRANSACTION, MAX_EXTRA_DATA_BYTES,
        MAX_TRANSACTIONS_PER_PAYLOAD,
    },
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    signing::verify_signature,
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
    Fork,
};
use helix_utils::signing::compute_builder_signing_root;

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
#[ssz(transparent)]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Deneb(SignedBidSubmissionDeneb),
    Capella(SignedBidSubmissionCapella),
}

impl BidSubmission for SignedBidSubmission {
    fn proofs(&self) -> Option<&InclusionProofs> {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.proofs.as_ref()
            }
            SignedBidSubmission::Capella(_) => None,
        }
    }

    fn bid_trace(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    fn signature(&self) -> &BlsSignature {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.signature,
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.signature,
        }
    }

    fn slot(&self) -> Slot {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => signed_bid_submission.message.slot,
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.slot
            }
        }
    }

    fn parent_hash(&self) -> &Hash32 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
        }
    }

    fn block_hash(&self) -> &Hash32 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
        }
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.builder_public_key
            }
        }
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_public_key
            }
        }
    }

    fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
        }
    }

    fn gas_used(&self) -> u64 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
        }
    }

    fn value(&self) -> U256 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
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
        }
    }

    fn withdrawals_root(&self) -> Option<Node> {
        match &self.execution_payload() {
            ExecutionPayload::Bellatrix(_) => None,
            ExecutionPayload::Capella(payload) => {
                let mut withdrawals = payload.withdrawals.clone();
                match withdrawals.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Deneb(payload) => {
                let mut withdrawals = payload.withdrawals.clone();
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
                let mut transactions = payload.transactions.clone();
                match transactions.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
            ExecutionPayload::Deneb(payload) => {
                let mut transactions = payload.transactions.clone();
                match transactions.hash_tree_root() {
                    Ok(root) => Some(root),
                    Err(_) => None,
                }
            }
        }
    }

    fn consensus_version(&self) -> Fork {
        match self {
            SignedBidSubmission::Deneb(_) => Fork::Deneb,
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
        let signing_root = compute_builder_signing_root(self.message_mut(), context)?;
        let public_key = self.builder_public_key();
        verify_signature(public_key, signing_root.as_ref(), self.signature())
    }

    pub fn transactions(
        &self,
    ) -> &List<ByteList<MAX_BYTES_PER_TRANSACTION>, MAX_TRANSACTIONS_PER_PAYLOAD> {
        match &self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions()
            }
        }
    }

    pub fn blobs_bundle(&self) -> Option<&BlobsBundle> {
        match &self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                Some(&signed_bid_submission.blobs_bundle)
            }
            SignedBidSubmission::Capella(_) => None,
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Capella(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => &mut signed_bid_submission.message,
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
        }
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
        }
    }

    pub fn execution_payload_mut(&mut self) -> &mut ExecutionPayload {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                &mut signed_bid_submission.execution_payload
            }
        }
    }

    pub fn execution_payload_moved(self) -> ExecutionPayload {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Capella(signed_bid_submission) => {
                signed_bid_submission.execution_payload
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        match self {
            SignedBidSubmission::Deneb(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: self.blobs_bundle().cloned(),
            },
            SignedBidSubmission::Capella(_) => PayloadAndBlobs {
                execution_payload: self.execution_payload().clone(),
                blobs_bundle: None,
            },
        }
    }

    pub fn num_blobs(&self) -> u64 {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.blobs_bundle.blobs.len() as u64
            }
            SignedBidSubmission::Capella(_) => 0,
        }
    }

    pub fn blob_gas_used(&self) -> u64 {
        match self.execution_payload() {
            ExecutionPayload::Deneb(payload) => payload.blob_gas_used,
            _ => 0,
        }
    }


    pub fn excess_blob_gas(&self) -> u64 {
        match self.execution_payload() {
            ExecutionPayload::Deneb(payload) => payload.excess_blob_gas,
            _ => 0,
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

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBidSubmissionCapella {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBidSubmissionDeneb {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
    /// The Merkle proofs of inclusion as needed by the Constraints API.
    /// Reference: <https://docs.boltprotocol.xyz/technical-docs/api/builder#get_header_with_proofs>
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<InclusionProofs>,
}
