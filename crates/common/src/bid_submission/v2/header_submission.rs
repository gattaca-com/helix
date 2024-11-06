use crate::{
    bid_submission::{BidSubmission, BidTrace},
    capella,
    deneb::{self, BlobsBundle},
    proofs::InclusionProofs,
    versioned_payload_header::VersionedExecutionPayloadHeader,
};
use ethereum_consensus::{
    altair::Bytes32,
    capella::Withdrawal,
    deneb::{
        mainnet::{BYTES_PER_LOGS_BLOOM, MAX_BLOB_COMMITMENTS_PER_BLOCK, MAX_EXTRA_DATA_BYTES},
        polynomial_commitments::KzgCommitment,
    },
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    signing::verify_signature,
    ssz::prelude::*,
    types::mainnet::ExecutionPayloadHeader,
    Fork,
};
use helix_utils::signing::compute_builder_signing_root;

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

#[derive(Default, Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmissionDenebV2 {
    pub bid_trace: BidTrace,
    pub execution_payload_header: deneb::ExecutionPayloadHeader,
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

// TODO: remove HeaderSubmissionDeneb when we roll out with just commitments
#[derive(Clone, Debug, SimpleSerialize, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
#[ssz(transparent)]
pub enum HeaderSubmissionMessage {
    V1(HeaderSubmissionDeneb),
    V2(HeaderSubmissionDenebV2),
}

impl Default for HeaderSubmissionMessage {
    fn default() -> Self {
        HeaderSubmissionMessage::V1(HeaderSubmissionDeneb::default())
    }
}

impl HeaderSubmissionMessage {
    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::V1(header_submission) => &header_submission.bid_trace,
            Self::V2(header_submission) => &header_submission.bid_trace,
        }
    }

    pub fn execution_payload_header(&self) -> &deneb::ExecutionPayloadHeader {
        match self {
            Self::V1(header_submission) => &header_submission.execution_payload_header,
            Self::V2(header_submission) => &header_submission.execution_payload_header,
        }
    }

    pub fn commitments(&self) -> Option<&List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>> {
        match self {
            Self::V1(header_submission) => Some(&header_submission.blobs_bundle.commitments),
            Self::V2(header_submission) => Some(&header_submission.commitments),
        }
    }
}

#[derive(Clone, Debug, SimpleSerialize, serde::Serialize, serde::Deserialize)]
#[ssz(transparent)]
#[serde(untagged)]
pub enum SignedHeaderSubmission {
    Deneb(SignedHeaderSubmissionDeneb),
    Capella(SignedHeaderSubmissionCapella),
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
    pub message: HeaderSubmissionMessage,
    pub signature: BlsSignature,
}

impl BidSubmission for SignedHeaderSubmission {
    fn proofs(&self) -> Option<&InclusionProofs> {
        None
    }

    fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.bid_trace,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.bid_trace(),
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
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.fee_recipient
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().fee_recipient
            }
        }
    }

    fn state_root(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.state_root
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().state_root
            }
        }
    }

    fn receipts_root(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.receipts_root
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().receipts_root
            }
        }
    }

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM> {
        match self {
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.logs_bloom
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().logs_bloom
            }
        }
    }

    fn prev_randao(&self) -> &Bytes32 {
        match self {
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.prev_randao
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().prev_randao
            }
        }
    }

    fn block_number(&self) -> u64 {
        match self {
            Self::Capella(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.block_number
            }
            Self::Deneb(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header().block_number
            }
        }
    }

    fn timestamp(&self) -> u64 {
        match self {
            Self::Capella(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.timestamp
            }
            Self::Deneb(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header().timestamp
            }
        }
    }

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES> {
        match self {
            Self::Capella(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.extra_data
            }
            Self::Deneb(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header().extra_data
            }
        }
    }

    fn base_fee_per_gas(&self) -> U256 {
        match self {
            Self::Capella(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.base_fee_per_gas
            }
            Self::Deneb(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header().base_fee_per_gas
            }
        }
    }

    fn withdrawals(&self) -> Option<&[Withdrawal]> {
        None
    }

    fn withdrawals_root(&self) -> Option<Node> {
        match self {
            Self::Capella(signed_header_submission) => {
                Some(signed_header_submission.message.execution_payload_header.withdrawals_root)
            }
            Self::Deneb(signed_header_submission) => {
                Some(signed_header_submission.message.execution_payload_header().withdrawals_root)
            }
        }
    }

    fn transactions_root(&self) -> Option<Node> {
        match self {
            Self::Capella(signed_header_submission) => {
                Some(signed_header_submission.message.execution_payload_header.transactions_root)
            }
            Self::Deneb(signed_header_submission) => {
                Some(signed_header_submission.message.execution_payload_header().transactions_root)
            }
        }
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
    pub fn verify_signature(
        &mut self,
        context: &ethereum_consensus::state_transition::Context,
    ) -> Result<(), ethereum_consensus::Error> {
        let mut bid_trace = self.bid_trace().clone();
        let signing_root = compute_builder_signing_root(&mut bid_trace, context)?;
        let public_key = &self.bid_trace().builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), self.signature())
    }

    pub fn execution_payload_header(&self) -> ExecutionPayloadHeader {
        match self {
            Self::Capella(signed_header_submission) => ExecutionPayloadHeader::Capella(
                signed_header_submission.message.execution_payload_header.clone(),
            ),
            Self::Deneb(signed_header_submission) => ExecutionPayloadHeader::Deneb(
                signed_header_submission.message.execution_payload_header().clone(),
            ),
        }
    }

    pub fn commitments(&self) -> Option<&List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>> {
        match self {
            Self::Capella(_) => None,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.commitments(),
        }
    }

    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::Capella(signed_header_submission) => &signed_header_submission.message.bid_trace,
            Self::Deneb(signed_header_submission) => signed_header_submission.message.bid_trace(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialise_signed_header_submission() {
        let bytes: Vec<u8> = vec![
            100, 0, 0, 0, 93, 31, 4, 4, 6, 1, 1, 24, 145, 34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
            0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 244, 0, 0, 0, 63, 3, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 134, 1, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 44, 66, 11, 77,
            22, 88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 1, 2, 4, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 1, 0, 0, 0, 0, 0, 0, 0, 167, 1, 0, 0, 0, 0, 0, 0, 37, 91, 0, 0, 0, 0, 0, 0, 29, 2,
            0, 0, 0, 0, 0, 0, 72, 2, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 22, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 35, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 86, 1, 0, 0, 0, 0, 0,
            0, 18, 9, 0, 0, 0, 0, 0, 0, 4, 5, 6, 1, 2, 4, 56, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ];

        let result = SignedHeaderSubmission::deserialize(&bytes);
        assert!(result.is_ok());
        println!("{:?}", result.unwrap());
    }
}
