mod bid_submission;
mod blobs;
mod clock;
mod error;
mod hydration;
mod spec;
mod test_utils;
mod validator;

use std::sync::Arc;

use alloy_primitives::B256;
pub use bid_submission::*;
pub use blobs::*;
pub use clock::*;
pub use error::*;
pub use hydration::*;
pub use lh_kzg::{KzgCommitment, KzgProof};
pub use lh_test_random::TestRandom;
pub use lh_types::{
    fork_name::ForkName, fork_versioned_response::ForkVersionDecode, payload::ExecPayload,
    test_utils::TestRandom, EthSpec, MainnetEthSpec, SignedRoot,
};
use lh_types::{BlindedPayload, FixedVector, VariableList};
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
pub type BlsPublicKeyBytes = alloy_rpc_types::beacon::BlsPublicKey;
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
// pub type BlobsBundle = lh_eth2::types::BlobsBundle<MainnetEthSpec>;
pub type BlobsBundle = crate::blobs::BlobsBundleV1;

// Publish block
pub type VersionedSignedProposal = SignedBlockContents;
pub type SignedBeaconBlock = lh_types::signed_beacon_block::SignedBeaconBlock<MainnetEthSpec>;
pub type SignedBeaconBlockElectra =
    lh_types::signed_beacon_block::SignedBeaconBlockElectra<MainnetEthSpec>;

// Beacon block
pub type BeaconBlockElectra = lh_types::beacon_block::BeaconBlockElectra<MainnetEthSpec>;
pub type BeaconBlockBodyElectra =
    lh_types::beacon_block_body::BeaconBlockBodyElectra<MainnetEthSpec>;

// Execution payload
pub type ExecutionPayloadHeader =
    lh_types::execution_payload_header::ExecutionPayloadHeader<MainnetEthSpec>;
pub type ExecutionPayloadHeaderRef<'a> =
    lh_types::execution_payload_header::ExecutionPayloadHeaderRef<'a, MainnetEthSpec>;
pub type ExecutionPayloadBellatrix =
    lh_types::execution_payload::ExecutionPayloadBellatrix<MainnetEthSpec>;
pub type ExecutionPayloadHeaderElectra =
    lh_types::execution_payload_header::ExecutionPayloadHeaderElectra<MainnetEthSpec>;
pub type ExecutionPayload = lh_types::execution_payload::ExecutionPayload<MainnetEthSpec>;
pub type ExecutionPayloadElectra =
    lh_types::execution_payload::ExecutionPayloadElectra<MainnetEthSpec>;

// Get header
pub type BuilderBid = lh_types::builder_bid::BuilderBid<MainnetEthSpec>;
pub type BuilderBidElectra = lh_types::builder_bid::BuilderBidElectra<MainnetEthSpec>;
pub type SignedBuilderBidInner = lh_types::builder_bid::SignedBuilderBid<MainnetEthSpec>;
// TODO: change names , below should be GetHeaderResponse
/// Response object of GET `/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey}`
pub type SignedBuilderBid = lh_types::ForkVersionedResponse<SignedBuilderBidInner>;

// Get payload
/// Request object of POST `/eth/v1/builder/blinded_blocks`
pub type SignedBlindedBeaconBlock =
    lh_types::signed_beacon_block::SignedBlindedBeaconBlock<MainnetEthSpec>; // TODO: maybe re implement this to avoid trait
pub type SignedBlindedBeaconBlockElectra = lh_types::signed_beacon_block::SignedBeaconBlockElectra<
    MainnetEthSpec,
    BlindedPayload<MainnetEthSpec>,
>;
pub type BlindedPayloadElectra = lh_types::payload::BlindedPayloadElectra<MainnetEthSpec>;
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

pub fn maybe_upgrade_execution_payload(
    payload: ExecutionPayload,
    fork_name: ForkName,
) -> ExecutionPayload {
    if fork_name.electra_enabled() {
        if let ExecutionPayload::Deneb(d) = payload {
            return ExecutionPayload::Electra(ExecutionPayloadElectra {
                parent_hash: d.parent_hash,
                fee_recipient: d.fee_recipient,
                state_root: d.state_root,
                receipts_root: d.receipts_root,
                logs_bloom: d.logs_bloom,
                prev_randao: d.prev_randao,
                block_number: d.block_number,
                gas_limit: d.gas_limit,
                gas_used: d.gas_used,
                timestamp: d.timestamp,
                extra_data: d.extra_data,
                base_fee_per_gas: d.base_fee_per_gas,
                block_hash: d.block_hash,
                transactions: d.transactions,
                withdrawals: d.withdrawals,
                blob_gas_used: d.blob_gas_used,
                excess_blob_gas: d.excess_blob_gas,
            });
        }
    }

    payload
}

pub fn mock_public_key_bytes() -> lh_types::PublicKeyBytes {
    lh_types::PublicKeyBytes::empty()
}

#[derive(Clone, Debug, PartialEq, Serialize, Encode)]
#[serde(untagged)]
#[ssz(enum_behaviour = "transparent")]
pub enum ExecutionPayloadRef<'a> {
    Bellatrix(&'a lh_types::ExecutionPayloadBellatrix<MainnetEthSpec>),
    Capella(&'a lh_types::ExecutionPayloadCapella<MainnetEthSpec>),
    Deneb(&'a lh_types::ExecutionPayloadDeneb<MainnetEthSpec>),
    Electra(&'a lh_types::ExecutionPayloadElectra<MainnetEthSpec>),
    Fulu(&'a lh_types::ExecutionPayloadFulu<MainnetEthSpec>),
}

impl<'a> From<&'a ExecutionPayload> for ExecutionPayloadRef<'a> {
    fn from(payload: &'a ExecutionPayload) -> Self {
        match payload {
            ExecutionPayload::Bellatrix(payload) => ExecutionPayloadRef::Bellatrix(payload),
            ExecutionPayload::Capella(payload) => ExecutionPayloadRef::Capella(payload),
            ExecutionPayload::Deneb(payload) => ExecutionPayloadRef::Deneb(payload),
            ExecutionPayload::Electra(payload) => ExecutionPayloadRef::Electra(payload),
            ExecutionPayload::Fulu(payload) => ExecutionPayloadRef::Fulu(payload),
        }
    }
}

impl ExecutionPayloadRef<'_> {
    pub fn clone_from_ref(&self) -> ExecutionPayload {
        match self {
            ExecutionPayloadRef::Bellatrix(payload) => {
                ExecutionPayload::Bellatrix((*payload).clone())
            }
            ExecutionPayloadRef::Capella(payload) => ExecutionPayload::Capella((*payload).clone()),
            ExecutionPayloadRef::Deneb(payload) => ExecutionPayload::Deneb((*payload).clone()),
            ExecutionPayloadRef::Electra(payload) => ExecutionPayload::Electra((*payload).clone()),
            ExecutionPayloadRef::Fulu(payload) => ExecutionPayload::Fulu((*payload).clone()),
        }
    }

    pub fn fork_name(&self) -> ForkName {
        match self {
            ExecutionPayloadRef::Bellatrix(_) => ForkName::Bellatrix,
            ExecutionPayloadRef::Capella(_) => ForkName::Capella,
            ExecutionPayloadRef::Deneb(_) => ForkName::Deneb,
            ExecutionPayloadRef::Electra(_) => ForkName::Electra,
            ExecutionPayloadRef::Fulu(_) => ForkName::Fulu,
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match self {
            ExecutionPayloadRef::Bellatrix(payload) => &payload.parent_hash.0,
            ExecutionPayloadRef::Capella(payload) => &payload.parent_hash.0,
            ExecutionPayloadRef::Deneb(payload) => &payload.parent_hash.0,
            ExecutionPayloadRef::Electra(payload) => &payload.parent_hash.0,
            ExecutionPayloadRef::Fulu(payload) => &payload.parent_hash.0,
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            ExecutionPayloadRef::Bellatrix(payload) => &payload.block_hash.0,
            ExecutionPayloadRef::Capella(payload) => &payload.block_hash.0,
            ExecutionPayloadRef::Deneb(payload) => &payload.block_hash.0,
            ExecutionPayloadRef::Electra(payload) => &payload.block_hash.0,
            ExecutionPayloadRef::Fulu(payload) => &payload.block_hash.0,
        }
    }

    pub fn gas_limit(&self) -> u64 {
        match &self {
            ExecutionPayloadRef::Bellatrix(payload) => payload.gas_limit,
            ExecutionPayloadRef::Capella(payload) => payload.gas_limit,
            ExecutionPayloadRef::Deneb(payload) => payload.gas_limit,
            ExecutionPayloadRef::Electra(payload) => payload.gas_limit,
            ExecutionPayloadRef::Fulu(payload) => payload.gas_limit,
        }
    }

    pub fn gas_used(&self) -> u64 {
        match &self {
            ExecutionPayloadRef::Bellatrix(payload) => payload.gas_used,
            ExecutionPayloadRef::Capella(payload) => payload.gas_used,
            ExecutionPayloadRef::Deneb(payload) => payload.gas_used,
            ExecutionPayloadRef::Electra(payload) => payload.gas_used,
            ExecutionPayloadRef::Fulu(payload) => payload.gas_used,
        }
    }

    pub fn transactions(&self) -> &Transactions {
        match &self {
            ExecutionPayloadRef::Bellatrix(payload) => &payload.transactions,
            ExecutionPayloadRef::Capella(payload) => &payload.transactions,
            ExecutionPayloadRef::Deneb(payload) => &payload.transactions,
            ExecutionPayloadRef::Electra(payload) => &payload.transactions,
            ExecutionPayloadRef::Fulu(payload) => &payload.transactions,
        }
    }
}
