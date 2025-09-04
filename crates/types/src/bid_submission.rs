use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use lh_test_random::TestRandom;
use lh_types::{test_utils::TestRandom, SignedRoot, Slot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, fields::ExecutionRequests, BlobsBundle, BlsPublicKey, BlsSignature, ChainSpec,
    ExecutionPayload, PayloadAndBlobsRef, ValidationError,
};

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash, TestRandom,
)]
#[serde(deny_unknown_fields)]
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
    /// The public key of the proposer. // TODO: use bytes
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

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: Arc<ExecutionPayload>,
    pub blobs_bundle: Arc<BlobsBundle>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignature,
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
    pub fn validate_payload_ssz_lengths(&self) -> Result<(), ValidationError> {
        match self {
            SignedBidSubmission::Electra(bid) => {
                bid.execution_payload.validate_ssz_lengths()?;
                bid.blobs_bundle.validate_ssz_lengths()?;
            }
        }

        Ok(())
    }

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

    pub fn execution_payload_ref(&self) -> &ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
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
