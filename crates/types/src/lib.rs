mod bid_adjustment_data;
mod bid_submission;
mod blobs;
mod block_merging;
mod builder_bid;
mod clock;
mod error;
mod execution_payload;
mod fields;
mod hydration;
mod test_utils;
mod utils;
mod validator;

pub use bid_adjustment_data::{BidAdjData, BidAdjDataV2, BidAdjustmentData, BidAdjustmentDataV1, BidAdjustmentDataV2};
pub use bid_submission::*;
pub use blobs::*;
pub use block_merging::*;
pub use builder_bid::*;
pub use clock::*;
pub use error::*;
pub use execution_payload::*;
pub use fields::*;
pub use hydration::*;
pub use lh_kzg::{KzgCommitment, KzgProof};
pub use lh_test_random::TestRandom;
pub use lh_types::{
    Config as LhConfig, EthSpec, ForkVersionDecode, MainnetEthSpec, SignedRoot,
    fork_name::ForkName, payload::ExecPayload, test_utils::TestRandom,
};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
pub use test_utils::*;
pub use validator::*;

pub type Slot = lh_types::Slot;
pub type Epoch = lh_types::Epoch;
pub type Domain = lh_types::Domain;
pub type ChainSpec = lh_types::ChainSpec;

// Signing
pub type BlsPublicKey = lh_types::PublicKey;
pub type BlsPublicKeyBytes = alloy_rpc_types::beacon::BlsPublicKey;
pub type BlsSignature = lh_types::Signature;
pub type BlsSignatureBytes = alloy_rpc_types::beacon::BlsSignature;
pub type BlsSecretKey = lh_types::SecretKey;
pub type BlsKeypair = lh_types::Keypair;

// Blobs
// pub type BlobsBundle = lh_eth2::types::BlobsBundle<MainnetEthSpec>;
pub type BlobsBundle = crate::blobs::BlobsBundle;

// Publish block
pub type VersionedSignedProposal = SignedBlockContents;
pub type SignedBeaconBlock = lh_types::signed_beacon_block::SignedBeaconBlock<MainnetEthSpec>;
pub type SignedBeaconBlockElectra =
    lh_types::signed_beacon_block::SignedBeaconBlockElectra<MainnetEthSpec>;
pub type SignedBeaconBlockFulu =
    lh_types::signed_beacon_block::SignedBeaconBlockFulu<MainnetEthSpec>;

// Beacon block
pub type BeaconBlockElectra = lh_types::beacon_block::BeaconBlockElectra<MainnetEthSpec>;
pub type BeaconBlockBodyElectra =
    lh_types::beacon_block_body::BeaconBlockBodyElectra<MainnetEthSpec>;
pub type BeaconBlockFulu = lh_types::beacon_block::BeaconBlockFulu<MainnetEthSpec>;
pub type BeaconBlockBodyFulu = lh_types::beacon_block_body::BeaconBlockBodyFulu<MainnetEthSpec>;

// Get header
pub type SignedBuilderBid = crate::builder_bid::SignedBuilderBid;
/// Response object of GET `/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey}`
pub type GetHeaderResponse = lh_types::ForkVersionedResponse<SignedBuilderBid>;

// Get payload
/// Request object of POST `/eth/v1/builder/blinded_blocks`
pub type SignedBlindedBeaconBlock =
    lh_types::signed_beacon_block::SignedBlindedBeaconBlock<MainnetEthSpec>; // TODO: maybe re implement this to avoid trait
pub type SignedBlindedBeaconBlockElectra =
    lh_types::signed_beacon_block::SignedBeaconBlockElectra<MainnetEthSpec, BlindedPayload>;
pub type SignedBlindedBeaconBlockFulu =
    lh_types::signed_beacon_block::SignedBeaconBlockFulu<MainnetEthSpec, BlindedPayload>;
pub type BlindedPayloadElectra = lh_types::payload::BlindedPayloadElectra<MainnetEthSpec>;
pub type BlindedPayload = lh_types::payload::BlindedPayload<MainnetEthSpec>;
pub type BlindedPayloadRef<'a> = lh_types::payload::BlindedPayloadRef<'a, MainnetEthSpec>;

/// Response object of POST `/eth/v1/builder/blinded_blocks`
pub type GetPayloadResponse = lh_types::ForkVersionedResponse<PayloadAndBlobs>;

// Registration
pub type ValidatorRegistration = validator::ValidatorRegistrationData;
/// Request object of POST `/eth/v1/builder/validators`
pub type SignedValidatorRegistration = validator::SignedValidatorRegistrationData;

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedMessage<T: ssz::Encode + ssz::Decode> {
    pub message: T,
    pub signature: BlsSignatureBytes,
}

pub fn mock_public_key_bytes() -> BlsPublicKeyBytes {
    BlsPublicKeyBytes::default()
}

pub fn spec_from_config(config: lh_types::Config) -> ChainSpec {
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}
