pub use ethereum_consensus::{builder::SignedValidatorRegistration, deneb::mainnet as spec};
use ethereum_consensus::{
    crypto::kzg::{KzgCommitment, KzgProof},
    deneb::mainnet::MAX_BLOB_COMMITMENTS_PER_BLOCK,
    primitives::{BlsPublicKey, BlsSignature, Root, U256},
    serde::as_str,
    ssz::prelude::*,
    types::mainnet::SignedBeaconBlock,
};

pub type ExecutionPayload = spec::ExecutionPayload;
pub type ExecutionPayloadHeader = spec::ExecutionPayloadHeader;
pub type SignedBlindedBeaconBlock = spec::SignedBlindedBeaconBlock;
pub type Blob = spec::Blob;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BuilderBid {
    pub header: spec::ExecutionPayloadHeader,
    pub blob_kzg_commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    #[serde(with = "as_str")]
    pub value: U256,
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BlindedBlobsBundle {
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blob_roots: List<Root, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedBuilderBid {
    pub message: BuilderBid,
    pub signature: BlsSignature,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BlobsBundle {
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Clone, Serializable, HashTreeRoot, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SignedBlockContents {
    pub signed_block: SignedBeaconBlock,
    pub kzg_proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}
