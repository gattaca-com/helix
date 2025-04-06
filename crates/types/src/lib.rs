mod bid_submission;
mod clock;
mod error;
mod spec;
mod test_utils;
mod validator;

use std::sync::Arc;

pub use bid_submission::*;
pub use clock::*;
pub use error::*;
pub use lh_kzg::KzgProof;
pub use lh_test_random::TestRandom;
pub use lh_types::{
    blob_sidecar::BlobSidecarError, fork_name::ForkName,
    fork_versioned_response::ForkVersionDecode, payload::ExecPayload, test_utils::TestRandom,
    MainnetEthSpec, SignedRoot,
};
use lh_types::{BlindedPayload, EthSpec, FixedVector, VariableList};
use serde::{Deserialize, Serialize};
pub use spec::*;
use ssz_derive::{Decode, Encode};
pub use test_utils::*;
pub use validator::*;

pub type Slot = lh_types::Slot;
pub type Epoch = lh_types::Epoch;
pub type Domain = lh_types::Domain;

// Signing
pub type BlsPublicKey = lh_types::PublicKey;
pub type BlsSignature = lh_types::Signature;
pub type BlsSecretKey = lh_types::SecretKey;
pub type BlsKeypair = lh_types::Keypair;

// Fields
pub type Withdrawal = lh_types::withdrawal::Withdrawal;
pub type Withdrawals = lh_types::execution_payload::Withdrawals<MainnetEthSpec>;
pub type Transaction =
    lh_types::execution_payload::Transaction<<MainnetEthSpec as EthSpec>::MaxBytesPerTransaction>;
pub type Transactions = lh_types::execution_payload::Transactions<MainnetEthSpec>;
pub type KzgCommitments = lh_types::beacon_block_body::KzgCommitments<MainnetEthSpec>;
pub type Bloom = FixedVector<u8, <MainnetEthSpec as EthSpec>::BytesPerLogsBloom>;
pub type ExtraData = VariableList<u8, <MainnetEthSpec as EthSpec>::MaxExtraDataBytes>;
pub type ExecutionRequests = lh_types::execution_requests::ExecutionRequests<MainnetEthSpec>;

// Blobs
pub type Blob = lh_types::Blob<MainnetEthSpec>;
pub type BlobSidecar = lh_types::blob_sidecar::BlobSidecar<MainnetEthSpec>;
pub type BlobSidecars = lh_types::blob_sidecar::BlobSidecarList<MainnetEthSpec>;
pub type BlobsBundle = lh_eth2::types::BlobsBundle<MainnetEthSpec>;

// Publish block
pub type VersionedSignedProposal = lh_eth2::types::SignedBlockContents<MainnetEthSpec>;
pub type SignedBlockContents = lh_eth2::types::SignedBlockContents<MainnetEthSpec>;
pub type SignedBeaconBlock = lh_types::signed_beacon_block::SignedBeaconBlock<MainnetEthSpec>;
pub type SignedBeaconBlockDeneb =
    lh_types::signed_beacon_block::SignedBeaconBlockDeneb<MainnetEthSpec>;
pub type SignedBeaconBlockElectra =
    lh_types::signed_beacon_block::SignedBeaconBlockElectra<MainnetEthSpec>;

// Beacon block
pub type BeaconBlockDeneb = lh_types::beacon_block::BeaconBlockDeneb<MainnetEthSpec>;
pub type BeaconBlockElectra = lh_types::beacon_block::BeaconBlockElectra<MainnetEthSpec>;
pub type BeaconBlockBodyDeneb = lh_types::beacon_block_body::BeaconBlockBodyDeneb<MainnetEthSpec>;
pub type BeaconBlockBodyElectra =
    lh_types::beacon_block_body::BeaconBlockBodyElectra<MainnetEthSpec>;

// Execution payload
pub type ExecutionPayloadHeader =
    lh_types::execution_payload_header::ExecutionPayloadHeader<MainnetEthSpec>;
pub type ExecutionPayloadBellatrix =
    lh_types::execution_payload::ExecutionPayloadBellatrix<MainnetEthSpec>;
pub type ExecutionPayloadHeaderDeneb =
    lh_types::execution_payload_header::ExecutionPayloadHeaderDeneb<MainnetEthSpec>;
pub type ExecutionPayloadHeaderElectra =
    lh_types::execution_payload_header::ExecutionPayloadHeaderElectra<MainnetEthSpec>;
pub type ExecutionPayload = lh_types::execution_payload::ExecutionPayload<MainnetEthSpec>;
pub type ExecutionPayloadDeneb = lh_types::execution_payload::ExecutionPayloadDeneb<MainnetEthSpec>;
pub type ExecutionPayloadElectra =
    lh_types::execution_payload::ExecutionPayloadElectra<MainnetEthSpec>;

// Get header
pub type BuilderBid = lh_types::builder_bid::BuilderBid<MainnetEthSpec>;
pub type BuilderBidDeneb = lh_types::builder_bid::BuilderBidDeneb<MainnetEthSpec>;
pub type BuilderBidElectra = lh_types::builder_bid::BuilderBidElectra<MainnetEthSpec>;
/// Response object of GET `/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey}`
pub type SignedBuilderBid = lh_types::builder_bid::SignedBuilderBid<MainnetEthSpec>;

// Get payload
/// Request object of POST `/eth/v1/builder/blinded_blocks`
pub type SignedBlindedBeaconBlock =
    lh_types::signed_beacon_block::SignedBlindedBeaconBlock<MainnetEthSpec>; // TODO: maybe re implement this to avoid trait
pub type SignedBlindedBeaconBlockDeneb = lh_types::signed_beacon_block::SignedBeaconBlockDeneb<
    MainnetEthSpec,
    BlindedPayload<MainnetEthSpec>,
>;
pub type SignedBlindedBeaconBlockElectra = lh_types::signed_beacon_block::SignedBeaconBlockElectra<
    MainnetEthSpec,
    BlindedPayload<MainnetEthSpec>,
>;
pub type PayloadAndBlobs = lh_eth2::types::ExecutionPayloadAndBlobs<MainnetEthSpec>;
/// Response object of POST `/eth/v1/builder/blinded_blocks`
pub type GetPayloadResponse = lh_types::ForkVersionedResponse<Arc<PayloadAndBlobs>>;

// Registration
pub type ValidatorRegistration = validator::ValidatorRegistrationData;
/// Request object of POST `/eth/v1/builder/validators`
pub type SignedValidatorRegistration = validator::SignedValidatorRegistrationData;

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedMessage<T: ssz::Encode + ssz::Decode> {
    pub message: T,
    pub signature: BlsSignature,
}

pub fn eth_consensus_hash_to_alloy(
    hash: &ethereum_consensus::primitives::Bytes32,
) -> alloy_primitives::B256 {
    alloy_primitives::B256::from_slice(hash.as_ref())
}

pub fn alloy_hash_to_eth_consensus(
    hash: &alloy_primitives::B256,
) -> ethereum_consensus::primitives::Bytes32 {
    ethereum_consensus::primitives::Bytes32::try_from(hash.as_ref()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eth_consensus_hash_to_alloy() {
        let hash = alloy_primitives::B256::random();
        let hash_2 = eth_consensus_hash_to_alloy(&alloy_hash_to_eth_consensus(&hash));
        assert_eq!(hash, hash_2);
    }

    #[test]
    fn t() {
        let x = std::mem::size_of::<BlsSignature>();
        println!("x: {}", x);
    }
}
