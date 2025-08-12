use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use lh_test_random::TestRandom;
use lh_types::{test_utils::TestRandom, ExecutionPayloadElectra, MainnetEthSpec, SignedRoot, Slot};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, BlobsBundle, BlockMergingData, BlsPublicKey, BlsSignature, ChainSpec,
    ExecutionPayloadRef, ExecutionRequests, PayloadAndBlobsRef,
};

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash, TestRandom,
)]
pub struct BidTrace {
    /// The slot associated with the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub slot: u64,
    /// The parent hash of the block.
    pub parent_hash: B256,
    /// The hash of the block.
    pub block_hash: B256,
    /// The public key of the builder.
    pub builder_pubkey: BlsPublicKey,
    /// The public key of the proposer.
    pub proposer_pubkey: BlsPublicKey,
    /// The recipient of the proposer's fee.
    pub proposer_fee_recipient: Address,
    /// The gas limit associated with the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    /// The gas used within the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    /// The value associated with the block.
    #[serde(with = "serde_utils::quoted_u256")]
    pub value: U256,
}

impl SignedRoot for BidTrace {}

impl BidTrace {
    pub fn slot(&self) -> Slot {
        Slot::from(self.slot)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TestRandom)]
#[serde(deny_unknown_fields)]
/// SSZ Encode and Decode are implemented manually inside the [`ssz_encoding`] module.
/// This enables us to accept submissions that don't specify a `merging_data` field.
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: Arc<ExecutionPayloadElectra<MainnetEthSpec>>,
    pub blobs_bundle: Arc<BlobsBundle>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignature,
    #[serde(default)]
    #[serde(skip_serializing_if = "BlockMergingData::is_default")]
    pub merging_data: BlockMergingData,
}

mod ssz_encoding {
    use ssz::Encode;

    use super::*;

    impl Encode for SignedBidSubmissionElectra {
        fn is_ssz_fixed_len() -> bool {
            false
        }

        fn ssz_append(&self, buf: &mut Vec<u8>) {
            SignedBidSubmissionElectraDecoder::from(self.clone()).ssz_append(buf);
        }

        fn ssz_bytes_len(&self) -> usize {
            SignedBidSubmissionElectraDecoder::from(self.clone()).ssz_bytes_len()
        }
    }

    impl Decode for SignedBidSubmissionElectra {
        fn is_ssz_fixed_len() -> bool {
            false
        }

        fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
            SignedBidSubmissionElectraDecoder::from_ssz_bytes(bytes).map(Into::into)
        }
    }

    #[derive(Debug, Clone, Encode, Decode)]
    #[ssz(enum_behaviour = "transparent")]
    enum SignedBidSubmissionElectraDecoder {
        WithMergingData(SignedBidSubmissionElectraWithMergingData),
        WithoutMergingData(SignedBidSubmissionElectraWithoutMergingData),
    }

    impl From<SignedBidSubmissionElectraDecoder> for SignedBidSubmissionElectra {
        fn from(value: SignedBidSubmissionElectraDecoder) -> Self {
            let value = match value {
                SignedBidSubmissionElectraDecoder::WithMergingData(v) => v,
                SignedBidSubmissionElectraDecoder::WithoutMergingData(v) => v.into(),
            };
            Self {
                message: value.message,
                execution_payload: value.execution_payload,
                blobs_bundle: value.blobs_bundle,
                execution_requests: value.execution_requests,
                signature: value.signature,
                merging_data: value.merging_data,
            }
        }
    }

    impl From<SignedBidSubmissionElectra> for SignedBidSubmissionElectraDecoder {
        fn from(value: SignedBidSubmissionElectra) -> Self {
            if value.merging_data.is_default() {
                Self::WithoutMergingData(SignedBidSubmissionElectraWithoutMergingData {
                    message: value.message,
                    execution_payload: value.execution_payload,
                    blobs_bundle: value.blobs_bundle,
                    execution_requests: value.execution_requests,
                    signature: value.signature,
                })
            } else {
                Self::WithMergingData(SignedBidSubmissionElectraWithMergingData {
                    message: value.message,
                    execution_payload: value.execution_payload,
                    blobs_bundle: value.blobs_bundle,
                    execution_requests: value.execution_requests,
                    signature: value.signature,
                    merging_data: value.merging_data,
                })
            }
        }
    }

    #[derive(Debug, Clone, Encode, Decode, TestRandom)]
    struct SignedBidSubmissionElectraWithoutMergingData {
        message: BidTrace,
        execution_payload: Arc<ExecutionPayloadElectra<MainnetEthSpec>>,
        blobs_bundle: Arc<BlobsBundle>,
        execution_requests: Arc<ExecutionRequests>,
        signature: BlsSignature,
    }

    #[derive(Debug, Clone, Encode, Decode, TestRandom)]
    struct SignedBidSubmissionElectraWithMergingData {
        message: BidTrace,
        execution_payload: Arc<ExecutionPayloadElectra<MainnetEthSpec>>,
        blobs_bundle: Arc<BlobsBundle>,
        execution_requests: Arc<ExecutionRequests>,
        signature: BlsSignature,
        merging_data: BlockMergingData,
    }

    impl From<SignedBidSubmissionElectraWithoutMergingData>
        for SignedBidSubmissionElectraWithMergingData
    {
        fn from(value: SignedBidSubmissionElectraWithoutMergingData) -> Self {
            Self {
                message: value.message,
                execution_payload: value.execution_payload,
                blobs_bundle: value.blobs_bundle,
                execution_requests: value.execution_requests,
                signature: value.signature,
                merging_data: BlockMergingData::default(),
            }
        }
    }
}

/// Request object of POST `/relay/v1/builder/blocks`
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Electra(SignedBidSubmissionElectra),
}

impl From<SignedBidSubmissionElectra> for SignedBidSubmission {
    fn from(value: SignedBidSubmissionElectra) -> Self {
        SignedBidSubmission::Electra(value)
    }
}

impl SignedBidSubmission {
    pub fn verify_signature(&self, spec: &ChainSpec) -> Result<(), SigError> {
        let domain = spec.get_builder_domain();
        let valid = match self {
            SignedBidSubmission::Electra(bid) => {
                let message = bid.message.signing_root(domain);
                bid.signature.verify(&bid.message.builder_pubkey, message)
            }
        };

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }

    pub fn num_txs(&self) -> usize {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions.len()
            }
        }
    }

    pub fn blobs_bundle(&self) -> Arc<BlobsBundle> {
        match &self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.blobs_bundle.clone()
            }
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
        }
    }

    pub fn merging_data(&self) -> &BlockMergingData {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.merging_data
            }
        }
    }

    pub fn merging_data_mut(&mut self) -> &mut BlockMergingData {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.merging_data
            }
        }
    }

    pub fn execution_payload_ref(&self) -> ExecutionPayloadRef {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                ExecutionPayloadRef::Electra(&signed_bid_submission.execution_payload)
            }
        }
    }

    pub fn payload_and_blobs_ref(&self) -> PayloadAndBlobsRef {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => PayloadAndBlobsRef {
                execution_payload: self.execution_payload_ref(),
                blobs_bundle: &signed_bid_submission.blobs_bundle,
            },
        }
    }

    pub fn execution_requests(&self) -> Option<Arc<ExecutionRequests>> {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(signed_bid_submission.execution_requests.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use ssz::Encode;

    use super::*;
    use crate::test_utils::{test_encode_decode_json, test_encode_decode_ssz};

    #[test]
    // from the relay API spec, adding the blob and the proposer_pubkey field
    fn electra_bid_submission() {
        let data_json = include_str!("testdata/signed-bid-submission-electra.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(&data_json);
        assert!(matches!(s, SignedBidSubmission::Electra(_)));
    }

    #[test]
    // from alloy
    fn electra_bid_submission_2() {
        let data_json = include_str!("testdata/signed-bid-submission-electra-2.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(&data_json);
        assert!(matches!(s, SignedBidSubmission::Electra(_)));

        let data_ssz = include_bytes!("testdata/signed-bid-submission-electra-2.bin");
        test_encode_decode_ssz::<SignedBidSubmission>(data_ssz);
        assert_eq!(data_ssz, s.as_ssz_bytes().as_slice());
    }
}
