use alloy_primitives::{Address, B256, U256};

use lh_types::{ExecutionPayloadDeneb, ExecutionPayloadElectra, MainnetEthSpec, SignedRoot, Slot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, BlobsBundle, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayload,
    ExecutionRequests, PayloadAndBlobs,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash)]
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

    #[cfg(test)]
    pub fn random_for_test() -> Self {
        use crate::random_bls_pubkey;

        Self {
            slot: 0,
            parent_hash: B256::ZERO,
            block_hash: B256::ZERO,
            builder_pubkey: random_bls_pubkey(),
            proposer_pubkey: random_bls_pubkey(),
            proposer_fee_recipient: Address::ZERO,
            gas_limit: 0,
            gas_used: 0,
            value: U256::ZERO,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionDeneb {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadDeneb<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadElectra<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub execution_requests: ExecutionRequests,
    pub signature: BlsSignature,
}

/// Request object of POST `/relay/v1/builder/blocks`
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[ssz(enum_behaviour = "transparent")]
#[tree_hash(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Deneb(SignedBidSubmissionDeneb),
    Electra(SignedBidSubmissionElectra),
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

    pub fn execution_payload(&self) -> ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload.clone().into()
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.clone().into()
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        PayloadAndBlobs {
            execution_payload: self.execution_payload().clone(),
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
