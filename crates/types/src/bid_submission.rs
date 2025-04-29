use alloy_primitives::{Address, B256, U256};
use lh_test_random::TestRandom;
use lh_types::{
    test_utils::TestRandom, ExecutionPayloadDeneb, ExecutionPayloadElectra, MainnetEthSpec,
    SignedRoot, Slot,
};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, BlobsBundle, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayloadRef,
    ExecutionPayloadRefMut, ExecutionRequests, PayloadAndBlobs,
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

impl BidTrace {
    pub fn slot(&self) -> Slot {
        Slot::from(self.slot)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionDeneb {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadDeneb<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadElectra<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub execution_requests: ExecutionRequests,
    pub signature: BlsSignature,
}

/// Request object of POST `/relay/v1/builder/blocks`
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Deneb(SignedBidSubmissionDeneb),
    Electra(SignedBidSubmissionElectra),
}

impl From<SignedBidSubmissionDeneb> for SignedBidSubmission {
    fn from(value: SignedBidSubmissionDeneb) -> Self {
        SignedBidSubmission::Deneb(value)
    }
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
            SignedBidSubmission::Deneb(bid) => {
                let message = bid.message.signing_root(domain);
                bid.signature.verify(&bid.message.builder_pubkey, message)
            }
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
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions.len()
            }
        }
    }

    pub fn blobs_bundle(&self) -> &BlobsBundle {
        match &self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.blobs_bundle
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.blobs_bundle
            }
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => &mut signed_bid_submission.message,
        }
    }

    pub fn execution_payload_mut(&mut self) -> ExecutionPayloadRefMut {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                ExecutionPayloadRefMut::Deneb(&mut signed_bid_submission.execution_payload)
            }
            SignedBidSubmission::Electra(signed_bid_submission) => {
                ExecutionPayloadRefMut::Electra(&mut signed_bid_submission.execution_payload)
            }
        }
    }

    pub fn execution_payload(&self) -> ExecutionPayloadRef {
        match self {
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                (&signed_bid_submission.execution_payload).into()
            }
            SignedBidSubmission::Electra(signed_bid_submission) => {
                (&signed_bid_submission.execution_payload).into()
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        PayloadAndBlobs {
            execution_payload: self.execution_payload().clone_from_ref(),
            blobs_bundle: self.blobs_bundle().clone(),
        }
    }

    pub fn execution_requests(&self) -> Option<&ExecutionRequests> {
        match self {
            SignedBidSubmission::Deneb(_) => None,
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(&signed_bid_submission.execution_requests)
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
    fn deneb_bid_submission() {
        let data_json = include_str!("testdata/signed-bid-submission-deneb.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(&data_json);
        assert!(matches!(s, SignedBidSubmission::Deneb(_)));
    }

    #[test]
    // from alloy
    fn deneb_bid_submission_2() {
        let data_json = include_str!("testdata/signed-bid-submission-deneb-2.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(&data_json);
        assert!(matches!(s, SignedBidSubmission::Deneb(_)));

        let data_ssz = include_bytes!("testdata/signed-bid-submission-deneb-2.ssz");
        test_encode_decode_ssz::<SignedBidSubmission>(data_ssz);
        assert_eq!(data_ssz, s.as_ssz_bytes().as_slice());
    }

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
