use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use lh_types::{test_utils::TestRandom, ForkName, SignedRoot, Slot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash::TreeHash;
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, fields::ExecutionRequests, BlobsBundle, BlobsBundleMut, BlobsBundleV1,
    BlobsBundleV2, Bloom, BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes,
    ExecutionPayload, ExtraData, PayloadAndBlobs, ValidationError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash)]
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
    pub builder_pubkey: BlsPublicKeyBytes,
    /// The public key of the proposer.
    pub proposer_pubkey: BlsPublicKeyBytes,
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

impl TestRandom for BidTrace {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            slot: u64::random_for_test(rng),
            parent_hash: B256::random_for_test(rng),
            block_hash: B256::random_for_test(rng),
            builder_pubkey: BlsPublicKeyBytes::random(),
            proposer_pubkey: BlsPublicKeyBytes::random(),
            proposer_fee_recipient: Address::random_for_test(rng),
            gas_limit: u64::random_for_test(rng),
            gas_used: u64::random_for_test(rng),
            value: U256::random_for_test(rng),
        }
    }
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
    pub blobs_bundle: Arc<BlobsBundleV1>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignatureBytes,
}

impl TestRandom for SignedBidSubmissionElectra {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng).into(),
            blobs_bundle: BlobsBundleV1::random_for_test(rng).into(),
            execution_requests: ExecutionRequests::random_for_test(rng).into(),
            signature: BlsSignatureBytes::random(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionFulu {
    pub message: BidTrace,
    pub execution_payload: Arc<ExecutionPayload>,
    pub blobs_bundle: Arc<BlobsBundleV2>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignatureBytes,
}

impl TestRandom for SignedBidSubmissionFulu {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng).into(),
            blobs_bundle: BlobsBundleV2::random_for_test(rng).into(),
            execution_requests: ExecutionRequests::random_for_test(rng).into(),
            signature: BlsSignatureBytes::random(),
        }
    }
}

/// Request object of POST `/relay/v1/builder/blocks`
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Electra(SignedBidSubmissionElectra),
    Fulu(SignedBidSubmissionFulu),
}

impl From<SignedBidSubmissionElectra> for SignedBidSubmission {
    fn from(value: SignedBidSubmissionElectra) -> Self {
        SignedBidSubmission::Electra(value)
    }
}

impl From<SignedBidSubmissionFulu> for SignedBidSubmission {
    fn from(value: SignedBidSubmissionFulu) -> Self {
        SignedBidSubmission::Fulu(value)
    }
}

impl SignedBidSubmission {
    pub fn validate_payload_ssz_lengths(
        &self,
        max_blobs_per_block: usize,
    ) -> Result<(), ValidationError> {
        match self {
            SignedBidSubmission::Electra(bid) => {
                bid.execution_payload.validate_ssz_lengths()?;
                bid.blobs_bundle.validate_ssz_lengths()?;
            }
            SignedBidSubmission::Fulu(bid) => {
                bid.execution_payload.validate_ssz_lengths()?;
                bid.blobs_bundle.validate_ssz_lengths(max_blobs_per_block)?;
            }
        }

        Ok(())
    }

    pub fn verify_signature(&self, builder_domain: B256) -> Result<(), SigError> {
        let valid = match self {
            SignedBidSubmission::Electra(bid) => {
                let uncompressed_builder_pubkey =
                    BlsPublicKey::deserialize(bid.message.builder_pubkey.as_slice())
                        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
                let uncompressed_signature = BlsSignature::deserialize(bid.signature.as_slice())
                    .map_err(|_| SigError::InvalidBlsSignatureBytes)?;

                let message = bid.message.signing_root(builder_domain);
                uncompressed_signature.verify(&uncompressed_builder_pubkey, message)
            }
            SignedBidSubmission::Fulu(bid) => {
                let uncompressed_builder_pubkey =
                    BlsPublicKey::deserialize(bid.message.builder_pubkey.as_slice())
                        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
                let uncompressed_signature = BlsSignature::deserialize(bid.signature.as_slice())
                    .map_err(|_| SigError::InvalidBlsSignatureBytes)?;

                let message = bid.message.signing_root(builder_domain);
                uncompressed_signature.verify(&uncompressed_builder_pubkey, message)
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
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions.len()
            }
        }
    }

    pub fn blobs_bundle(&self) -> Arc<BlobsBundle> {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Arc::new(BlobsBundle::V1(signed_bid_submission.blobs_bundle.clone()))
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                Arc::new(BlobsBundle::V2(signed_bid_submission.blobs_bundle.clone()))
            }
        }
    }

    pub fn blobs_bundle_mut(&self) -> BlobsBundleMut {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                BlobsBundleMut::V1((*signed_bid_submission.blobs_bundle).clone())
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                BlobsBundleMut::V2((*signed_bid_submission.blobs_bundle).clone())
            }
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Fulu(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => &mut signed_bid_submission.message,
        }
    }

    pub fn execution_payload_ref(&self) -> &ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.execution_payload
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => PayloadAndBlobs {
                execution_payload: (*signed_bid_submission.execution_payload).clone(),
                blobs_bundle: BlobsBundle::V1(signed_bid_submission.blobs_bundle.clone()),
            },
            SignedBidSubmission::Fulu(signed_bid_submission) => PayloadAndBlobs {
                execution_payload: (*signed_bid_submission.execution_payload).clone(),
                blobs_bundle: BlobsBundle::V2(signed_bid_submission.blobs_bundle.clone()),
            },
        }
    }

    pub fn execution_requests(&self) -> Option<Arc<ExecutionRequests>> {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(signed_bid_submission.execution_requests.clone())
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                Some(signed_bid_submission.execution_requests.clone())
            }
        }
    }
}

impl SignedBidSubmission {
    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Fulu(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn signature(&self) -> &BlsSignatureBytes {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.signature,
            SignedBidSubmission::Fulu(signed_bid_submission) => &signed_bid_submission.signature,
        }
    }

    pub fn slot(&self) -> Slot {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.slot()
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                signed_bid_submission.message.slot()
            }
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
        }
    }

    pub fn builder_public_key(&self) -> &BlsPublicKeyBytes {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.builder_pubkey
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.message.builder_pubkey
            }
        }
    }

    pub fn proposer_public_key(&self) -> &BlsPublicKeyBytes {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_pubkey
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_pubkey
            }
        }
    }

    pub fn proposer_fee_recipient(&self) -> &Address {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
        }
    }

    pub fn gas_limit(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
        }
    }

    pub fn gas_used(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
        }
    }

    pub fn value(&self) -> U256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
            SignedBidSubmission::Fulu(signed_bid_submission) => signed_bid_submission.message.value,
        }
    }

    pub fn fee_recipient(&self) -> Address {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.fee_recipient,
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.fee_recipient,
        }
    }

    pub fn state_root(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.state_root,
            SignedBidSubmission::Fulu(bid) => &bid.execution_payload.state_root,
        }
    }

    pub fn receipts_root(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.receipts_root,
            SignedBidSubmission::Fulu(bid) => &bid.execution_payload.receipts_root,
        }
    }

    pub fn logs_bloom(&self) -> &Bloom {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.logs_bloom,
            SignedBidSubmission::Fulu(bid) => &bid.execution_payload.logs_bloom,
        }
    }

    pub fn prev_randao(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.prev_randao,
            SignedBidSubmission::Fulu(bid) => &bid.execution_payload.prev_randao,
        }
    }

    pub fn block_number(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.block_number,
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.block_number,
        }
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.timestamp,
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.timestamp,
        }
    }

    pub fn extra_data(&self) -> &ExtraData {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.extra_data,
            SignedBidSubmission::Fulu(bid) => &bid.execution_payload.extra_data,
        }
    }

    pub fn base_fee_per_gas(&self) -> U256 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.base_fee_per_gas,
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.base_fee_per_gas,
        }
    }

    pub fn withdrawals_root(&self) -> B256 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.withdrawals.tree_hash_root(),
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.withdrawals.tree_hash_root(),
        }
    }

    pub fn transactions_root(&self) -> B256 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.transaction_root(),
            SignedBidSubmission::Fulu(bid) => bid.execution_payload.transaction_root(),
        }
    }

    pub fn validate(&self) -> Result<(), super::BidValidationError> {
        let bid_trace = self.bid_trace();
        let execution_payload = self.execution_payload_ref();

        if bid_trace.parent_hash != execution_payload.parent_hash {
            return Err(BidValidationError::ParentHashMismatch {
                message: bid_trace.parent_hash,
                payload: execution_payload.parent_hash,
            });
        }

        if bid_trace.block_hash != execution_payload.block_hash {
            return Err(BidValidationError::BlockHashMismatch {
                message: bid_trace.block_hash,
                payload: execution_payload.block_hash,
            });
        }

        if bid_trace.gas_limit != execution_payload.gas_limit {
            return Err(BidValidationError::GasLimitMismatch {
                message: bid_trace.gas_limit,
                payload: execution_payload.gas_limit,
            });
        }

        if bid_trace.gas_used != execution_payload.gas_used {
            return Err(BidValidationError::GasUsedMismatch {
                message: bid_trace.gas_used,
                payload: execution_payload.gas_used,
            });
        }

        if bid_trace.value == U256::ZERO {
            return Err(BidValidationError::ZeroValueBlock);
        }

        Ok(())
    }

    pub fn fork_name(&self) -> ForkName {
        match self {
            SignedBidSubmission::Electra(_) => ForkName::Electra,
            SignedBidSubmission::Fulu(_) => ForkName::Fulu,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BidValidationError {
    #[error("block hash mismatch: message: {message:?}, payload: {payload:?}")]
    BlockHashMismatch { message: B256, payload: B256 },

    #[error("parent hash mismatch. message: {message:?}, payload: {payload:?}")]
    ParentHashMismatch { message: B256, payload: B256 },

    #[error("gas limit mismatch. message: {message:?}, payload: {payload:?}")]
    GasLimitMismatch { message: u64, payload: u64 },

    #[error("gas used mismatch. message: {message:?}, payload: {payload:?}")]
    GasUsedMismatch { message: u64, payload: u64 },

    #[error("zero value block")]
    ZeroValueBlock,
}

#[cfg(test)]
mod tests {

    use ssz::Encode;

    use super::*;
    use crate::test_utils::{test_decode_json, test_encode_decode_json, test_encode_decode_ssz};

    #[test]
    // from the relay API spec, adding the blob and the proposer_pubkey field
    fn electra_bid_submission() {
        let data_json = include_str!("testdata/signed-bid-submission-electra.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(data_json);
        match &s {
            SignedBidSubmission::Electra(_) => println!("Got Electra variant for JSON"),
            SignedBidSubmission::Fulu(_) => println!("Got Fulu variant for JSON (wrong!)"),
        }
        assert!(matches!(s, SignedBidSubmission::Electra(_)));
    }

    #[test]
    // from alloy
    fn electra_bid_submission_2() {
        let data_json = include_str!("testdata/signed-bid-submission-electra-2.json");
        let s = test_decode_json::<SignedBidSubmission>(data_json);

        let blobs_empty = match &s {
            SignedBidSubmission::Electra(electra) => electra.blobs_bundle.blobs.is_empty(),
            SignedBidSubmission::Fulu(fulu) => fulu.blobs_bundle.blobs.is_empty(),
        };

        if blobs_empty {
            // When blobs are empty, we can't distinguish variants reliably
            assert!(matches!(s, SignedBidSubmission::Electra(_) | SignedBidSubmission::Fulu(_)));
        } else {
            match &s {
                SignedBidSubmission::Electra(_) => println!("Got Electra variant for JSON"),
                SignedBidSubmission::Fulu(_) => {
                    println!("Got Fulu variant for JSON (may be wrong!)")
                }
            }
            assert!(matches!(s, SignedBidSubmission::Electra(_)));
        }

        let data_ssz = include_bytes!("testdata/signed-bid-submission-electra-2.bin");
        let s = test_encode_decode_ssz::<SignedBidSubmission>(data_ssz);
        assert_eq!(data_ssz, s.as_ssz_bytes().as_slice());
    }

    #[test]
    fn fulu_bid_submission_json() {
        let data_json = include_str!("testdata/signed-bid-submission-fulu.json");
        let s = test_decode_json::<SignedBidSubmission>(data_json);
        assert!(matches!(s, SignedBidSubmission::Fulu(_)));
    }

    #[test]
    fn fulu_bid_submission_ssz() {
        let data_ssz = include_bytes!("testdata/signed-bid-submission-fulu.ssz");
        let s = test_encode_decode_ssz::<SignedBidSubmission>(data_ssz);
        assert_eq!(data_ssz, s.as_ssz_bytes().as_slice());
    }
}
