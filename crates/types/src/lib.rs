mod error;
mod signed_bid_submission;
mod validator_registration;

use std::sync::Arc;
use std::{path::Path, time::Duration};

pub use lh_slot_clock::SlotClock as SlotClockTrait;
use lh_types::{EthSpec, FixedVector, VariableList};

pub use lh_types::MainnetEthSpec;

pub use lh_kzg::KzgProof;
pub use lh_types::blob_sidecar::BlobSidecarError;
pub use lh_types::SignedRoot;
pub use signed_bid_submission::*;

pub type SszError = ssz_types::Error;
pub type CryptoError = lh_bls::Error;
pub type SigError = error::SigError;

use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

pub use lh_slot_clock::SystemTimeSlotClock as SlotClock;

pub type Slot = lh_types::Slot;
pub type Epoch = lh_types::Epoch;

pub type Domain = lh_types::Domain;

pub use lh_types::fork_name::ForkName;
pub use lh_types::fork_versioned_response::ForkVersionDecode;

// TODO: maybe re implement this to avoid trait
pub type SignedBlindedBeaconBlock =
    lh_types::signed_beacon_block::SignedBlindedBeaconBlock<MainnetEthSpec>;

pub use lh_types::payload::ExecPayload;

pub type Withdrawal = lh_types::withdrawal::Withdrawal;
pub type Withdrawals = lh_types::execution_payload::Withdrawals<MainnetEthSpec>;
pub type Transaction =
    lh_types::execution_payload::Transaction<<MainnetEthSpec as EthSpec>::MaxBytesPerTransaction>;
pub type Transactions = lh_types::execution_payload::Transactions<MainnetEthSpec>;
pub type KzgCommittments = lh_types::beacon_block_body::KzgCommitments<MainnetEthSpec>;
pub type Blob = lh_types::Blob<MainnetEthSpec>;
pub type BlobSidecar = lh_types::blob_sidecar::BlobSidecar<MainnetEthSpec>;

pub type Bloom = FixedVector<u8, <MainnetEthSpec as EthSpec>::BytesPerLogsBloom>;

pub type BlobSidecars = lh_types::blob_sidecar::BlobSidecarList<MainnetEthSpec>;

pub type SignedBlockContents = lh_eth2::types::SignedBlockContents<MainnetEthSpec>;

pub type SignedBeaconBlock = lh_types::signed_beacon_block::SignedBeaconBlock<MainnetEthSpec>;
pub type SignedBeaconBlockDeneb =
    lh_types::signed_beacon_block::SignedBeaconBlockDeneb<MainnetEthSpec>;
pub type SignedBeaconBlockElectra =
    lh_types::signed_beacon_block::SignedBeaconBlockElectra<MainnetEthSpec>;

pub type BeaconBlockDeneb = lh_types::beacon_block::BeaconBlockDeneb<MainnetEthSpec>;
pub type BeaconBlockElectra = lh_types::beacon_block::BeaconBlockElectra<MainnetEthSpec>;
pub type BeaconBlockBodyDeneb = lh_types::beacon_block_body::BeaconBlockBodyDeneb<MainnetEthSpec>;
pub type BeaconBlockBodyElectra =
    lh_types::beacon_block_body::BeaconBlockBodyElectra<MainnetEthSpec>;

pub type ExecutionPayloadHeader =
    lh_types::execution_payload_header::ExecutionPayloadHeader<MainnetEthSpec>;

pub type ExecutionPayloadHeaderDeneb =
    lh_types::execution_payload_header::ExecutionPayloadHeaderDeneb<MainnetEthSpec>;

pub type ExecutionPayloadHeaderElectra =
    lh_types::execution_payload_header::ExecutionPayloadHeaderElectra<MainnetEthSpec>;

pub type ExecutionPayloadBellatrix =
    lh_types::execution_payload::ExecutionPayloadBellatrix<MainnetEthSpec>;
pub type ExecutionPayload = lh_types::execution_payload::ExecutionPayload<MainnetEthSpec>;

pub type ExecutionRequests = lh_types::execution_requests::ExecutionRequests<MainnetEthSpec>;

pub type BuilderBid = lh_types::builder_bid::BuilderBid<MainnetEthSpec>;
pub type BuilderBidDeneb = lh_types::builder_bid::BuilderBidDeneb<MainnetEthSpec>;
pub type BuilderBidElectra = lh_types::builder_bid::BuilderBidElectra<MainnetEthSpec>;

/// Response object of GET `/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey}`
pub type SignedBuilderBid = lh_types::builder_bid::SignedBuilderBid<MainnetEthSpec>;
/// Response object of POST `/eth/v1/builder/blinded_blocks`
pub type GetPayloadResponse = lh_types::ForkVersionedResponse<Arc<PayloadAndBlobs>>;
pub type PayloadAndBlobs = lh_eth2::types::ExecutionPayloadAndBlobs<MainnetEthSpec>;

pub type SignedValidatorRegistration = validator_registration::SignedValidatorRegistrationData;
pub type ValidatorRegistration = validator_registration::ValidatorRegistrationData;

pub type Validator = validator_registration::Validator;

pub type BlobsBundle = lh_eth2::types::BlobsBundle<MainnetEthSpec>;

pub type ExtraData = VariableList<u8, <MainnetEthSpec as EthSpec>::MaxExtraDataBytes>;

pub type VersionedSignedProposal = lh_eth2::types::SignedBlockContents<MainnetEthSpec>;

pub type BlsPublicKey = lh_types::PublicKey;
pub type BlsSignature = lh_types::Signature;
pub type BlsSecretKey = lh_types::SecretKey;
pub type BlsKeypair = lh_types::Keypair;
pub type BlsPublicKeyBytes = lh_types::PublicKeyBytes;

pub type ChainSpec = lh_types::ChainSpec;

pub fn get_payload_response_from_un() {}

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedMessage<T: ssz::Encode + ssz::Decode> {
    pub message: T,
    pub signature: BlsSignature,
}

pub fn eth_consensus_pubkey_to_alloy(
    pubkey: &ethereum_consensus::primitives::BlsPublicKey,
) -> BlsPublicKey {
    BlsPublicKey::deserialize(pubkey.as_ref()).unwrap()
}

pub fn alloy_pubkey_to_eth_consensus(
    pubkey: &BlsPublicKey,
) -> ethereum_consensus::primitives::BlsPublicKey {
    ethereum_consensus::primitives::BlsPublicKey::try_from(pubkey.serialize().as_slice()).unwrap()
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
pub fn random_bls_pubkey() -> BlsPublicKey {
    use lh_types::test_utils::{TestRandom, XorShiftRng};
    use rand::SeedableRng;

    let rng = &mut XorShiftRng::from_seed([42; 16]);
    BlsPublicKey::random_for_test(rng)
}

pub const MAINNET_GENESIS_TIME: u64 = 1606824023;
pub const SEPOLIA_GENESIS_TIME: u64 = 1655733600;
pub const HOLESKY_GENESIS_TIME: u64 = 1695902400;
pub const SECONDS_PER_SLOT: u64 = 12;
pub const SLOTS_PER_EPOCH: u64 = 32;

pub fn mainnet_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(MAINNET_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn sepolia_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(SEPOLIA_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn holesky_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(HOLESKY_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn custom_slot_clock(genesis_time: u64, seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(genesis_time),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn sepolia_spec() -> ChainSpec {
    let spec = include_bytes!("specs/sepolia_spec.json");
    let config: lh_types::Config = serde_json::from_slice(spec).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

pub fn holesky_spec() -> ChainSpec {
    let spec = include_bytes!("specs/holesky_spec.json");
    let config: lh_types::Config = serde_json::from_slice(spec).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

pub fn spec_from_file<P: AsRef<Path>>(path: P) -> ChainSpec {
    let file = std::fs::File::open(path).unwrap();
    let config: lh_types::Config = serde_json::from_reader(file).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eth_consensus_pubkey_to_alloy() {
        let pubkey = random_bls_pubkey();
        let pubkey_2 = eth_consensus_pubkey_to_alloy(&alloy_pubkey_to_eth_consensus(&pubkey));
        assert_eq!(pubkey, pubkey_2);
    }

    #[test]
    fn test_eth_consensus_hash_to_alloy() {
        let hash = alloy_primitives::B256::random();
        let hash_2 = eth_consensus_hash_to_alloy(&alloy_hash_to_eth_consensus(&hash));
        assert_eq!(hash, hash_2);
    }
}
