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
use crate::{bid_submission::{BidTrace, BidSubmission}, versioned_payload_header::VersionedExecutionPayloadHeader, deneb::{BlobsBundle, self}, capella};


#[derive(Default, Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmission {
    pub bid_trace: BidTrace,
    #[serde(flatten)]
    pub versioned_execution_payload: VersionedExecutionPayloadHeader,
}

#[derive(Default, Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmissionCapella {
    pub bid_trace: BidTrace,
    pub execution_payload_header: capella::ExecutionPayloadHeader,
}

#[derive(Default, Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmissionDeneb {
    pub bid_trace: BidTrace,
    pub execution_payload_header: deneb::ExecutionPayloadHeader,
    pub blobs_bundle: BlobsBundle,
}

#[derive(Clone, Debug, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub enum SignedHeaderSubmission {
    Capella(SignedHeaderSubmissionCapella),
    Deneb(SignedHeaderSubmissionDeneb),
}

impl Default for SignedHeaderSubmission {
    fn default() -> Self {
        Self::Capella(SignedHeaderSubmissionCapella::default())
    }
}

#[derive(Clone, Debug, Default, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedHeaderSubmissionCapella {
    pub message: HeaderSubmissionCapella,
    pub signature: BlsSignature,
}

#[derive(Clone, Debug, Default, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedHeaderSubmissionDeneb {
    pub message: HeaderSubmissionDeneb,
    pub signature: BlsSignature,
}

impl BidSubmission for SignedHeaderSubmission {
    fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.bid_trace,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.bid_trace,
        }
    }

    fn signature(&self) -> &BlsSignature {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.signature,
            Self::Deneb(signed_header_submission) => &signed_header_submission.signature,
        }
    }

    fn slot(&self) -> Slot {
        self.bid_trace().slot
    }

    fn parent_hash(&self) -> &Hash32 {
        &self.bid_trace().parent_hash
    }

    fn block_hash(&self) -> &Hash32 {
        &self.bid_trace().block_hash
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        &self.bid_trace().builder_public_key
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.bid_trace().proposer_public_key
    }

    fn proposer_fee_recipient(&self) -> &ExecutionAddress {
        &self.bid_trace().proposer_fee_recipient
    }

    fn gas_limit(&self) -> u64 {
        self.bid_trace().gas_limit
    }

    fn gas_used(&self) -> u64 {
        self.bid_trace().gas_used
    }

    fn value(&self) -> U256 {
        self.bid_trace().value
    }

    fn fee_recipient(&self) -> &ExecutionAddress {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.fee_recipient,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.fee_recipient,
        }
    }

    fn state_root(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.state_root,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.state_root,
        }
    }

    fn receipts_root(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.receipts_root,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.receipts_root,
        }
    }

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.logs_bloom,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.logs_bloom,
        }
    }

    fn prev_randao(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.prev_randao,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.prev_randao,
        }
    }

    fn block_number(&self) -> u64 {
        match self {
            Self::Capella(signed_header_submission) => signed_header_submission.message.execution_payload_header.block_number,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.execution_payload_header.block_number,
        }
    }

    fn timestamp(&self) -> u64 {
        match self {
            Self::Capella(signed_header_submission) => signed_header_submission.message.execution_payload_header.timestamp,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.execution_payload_header.timestamp,
        }
    }

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.execution_payload_header.extra_data,
            Self::Deneb(signed_header_submission) => &signed_header_submission.message.execution_payload_header.extra_data,
        }
    }

    fn base_fee_per_gas(&self) -> U256 {
        match self {
            Self::Capella(signed_header_submission) => signed_header_submission.message.execution_payload_header.base_fee_per_gas,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.execution_payload_header.base_fee_per_gas,
        }
    }

    fn withdrawals(&self) -> Option<&[Withdrawal]> {
        None
    }

    fn consensus_version(&self) -> Fork {
        match self {
            Self::Capella(_) => Fork::Capella,
            Self::Deneb(_) => Fork::Deneb,
        }
    }

    fn is_full_payload(&self) -> bool {
        false
    }
}

impl SignedHeaderSubmission {
    pub fn verify_signature(&mut self, context: &ethereum_consensus::state_transition::Context) -> Result<(), ethereum_consensus::Error> {
        let signing_root = compute_builder_signing_root(self.bid_trace_mut(), context)?;
        let public_key = &self.bid_trace().builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), self.signature())
    }

    pub fn execution_payload_header(&self) -> ExecutionPayloadHeader {
        match self {
            Self::Capella(signed_header_submission) => {
                ExecutionPayloadHeader::Capella(signed_header_submission.message.execution_payload_header.clone())
            },
            Self::Deneb(signed_header_submission) => {
                ExecutionPayloadHeader::Deneb(signed_header_submission.message.execution_payload_header.clone())
            },
        }
    }

    pub fn blobs_bundle(&self) -> Option<&BlobsBundle> {
        match self {
            Self::Capella(_) => None,
            Self::Deneb(signed_header_submission) => Some(&signed_header_submission.message.blobs_bundle),
        }
    }

    pub fn bid_trace_mut(&mut self) -> &mut BidTrace {
        match self {
            Self::Capella(signed_header_submission) => &mut signed_header_submission.message.bid_trace,
            Self::Deneb(signed_header_submission) => &mut signed_header_submission.message.bid_trace,
        }
    }
}
