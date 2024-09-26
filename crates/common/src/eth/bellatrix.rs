use ethereum_consensus::primitives::{BlsPublicKey, BlsSignature};
pub use ethereum_consensus::{bellatrix::mainnet as spec, builder::SignedValidatorRegistration, serde::as_str, ssz::prelude::*};

pub type ExecutionPayload = spec::ExecutionPayload;
pub type ExecutionPayloadHeader = spec::ExecutionPayloadHeader;
pub type SignedBlindedBeaconBlock = spec::SignedBlindedBeaconBlock;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BuilderBid {
    pub header: spec::ExecutionPayloadHeader,
    #[serde(with = "as_str")]
    pub value: U256,
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBuilderBid {
    pub message: BuilderBid,
    pub signature: BlsSignature,
}
