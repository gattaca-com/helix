use std::sync::Arc;

use lh_types::{
    test_utils::TestRandom, EthSpec, FixedVector, ForkName, ForkVersionDecode, Hash256,
    MainnetEthSpec, SignedBeaconBlockHeader,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use ssz::DecodeError;
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{ExecutionPayload, ExecutionPayloadRef, SignedBeaconBlock};

pub type KzgCommitment = alloy_consensus::Bytes48;
pub type KzgCommitments = Vec<KzgCommitment>;
pub type KzgProof = alloy_consensus::Bytes48;
pub type KzgProofs = Vec<KzgProof>;
pub type Blob = Arc<alloy_consensus::Blob>;
pub type Blobs = Vec<Blob>;
pub type LhKzgCommitment = lh_types::KzgCommitment;

/// This includes all bundled blob related data of an executed payload.
/// From [`alloy_rpc_types_engine::BlobsBundleV1`]
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
pub struct BlobsBundleV1 {
    /// All commitments in the bundle.
    pub commitments: KzgCommitments,
    /// All proofs in the bundle.
    pub proofs: KzgProofs,
    /// All blobs in the bundle.
    pub blobs: Blobs,
}

impl TestRandom for BlobsBundleV1 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let max = MainnetEthSpec::max_blob_commitments_per_block();
        let n = rng.gen_range(0..=max);

        let commitments = (0..n).map(|_| KzgCommitment::random()).collect();
        let proofs = (0..n).map(|_| KzgProof::random()).collect();
        let blobs = (0..n).map(|_| Arc::new(alloy_consensus::Blob::random())).collect();

        BlobsBundleV1 { commitments, proofs, blobs }
    }
}

impl BlobsBundleV1 {
    pub fn validate(&self) -> Result<(), BlobsError> {
        if self.commitments.len() != self.proofs.len() || self.proofs.len() != self.blobs.len() {
            return Err(BlobsError::BundleMismatch {
                proofs: self.proofs.len(),
                commitments: self.commitments.len(),
                blobs: self.blobs.len(),
            });
        }

        if self.commitments.len() > MainnetEthSpec::max_blob_commitments_per_block() {
            return Err(BlobsError::BundleTooLarge {
                got: self.commitments.len(),
                max: MainnetEthSpec::max_blob_commitments_per_block(),
            });
        }

        Ok(())
    }
}

/// Similar to lighthouse but using our BlobsBundleV1
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode)]
pub struct PayloadAndBlobs {
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundleV1,
}

// From lighthouse
impl ForkVersionDecode for PayloadAndBlobs {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork_name: ForkName) -> Result<Self, DecodeError> {
        let mut builder = ssz::SszDecoderBuilder::new(bytes);
        builder.register_anonymous_variable_length_item()?;
        builder.register_type::<BlobsBundleV1>()?;
        let mut decoder = builder.build()?;

        if fork_name.deneb_enabled() {
            let execution_payload = decoder.decode_next_with(|bytes| {
                ExecutionPayload::from_ssz_bytes_by_fork(bytes, fork_name)
            })?;
            let blobs_bundle = decoder.decode_next()?;
            Ok(Self { execution_payload, blobs_bundle })
        } else {
            Err(DecodeError::BytesInvalid(format!(
                "ExecutionPayloadAndBlobs decoding for {fork_name} not implemented"
            )))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Encode)]
pub struct PayloadAndBlobsRef<'a> {
    pub execution_payload: ExecutionPayloadRef<'a>,
    pub blobs_bundle: &'a BlobsBundleV1,
}

impl<'a> From<&'a PayloadAndBlobs> for PayloadAndBlobsRef<'a> {
    fn from(payload_and_blobs: &'a PayloadAndBlobs) -> Self {
        let execution_payload = ExecutionPayloadRef::from(&payload_and_blobs.execution_payload);
        PayloadAndBlobsRef { execution_payload, blobs_bundle: &payload_and_blobs.blobs_bundle }
    }
}

impl PayloadAndBlobsRef<'_> {
    /// Clone out an owned `PayloadAndBlobs`
    pub fn to_owned(&self) -> PayloadAndBlobs {
        let execution_payload = self.execution_payload.clone_from_ref();
        let blobs_bundle = (*self.blobs_bundle).clone();
        PayloadAndBlobs { execution_payload, blobs_bundle }
    }
}

// From lighthouse, replacing the blobs and kzg proofs
#[derive(Debug, Clone, Serialize, Deserialize, Encode)]
pub struct SignedBlockContents {
    pub signed_block: Arc<SignedBeaconBlock>,
    pub kzg_proofs: KzgProofs,
    pub blobs: Blobs,
}

// From lighthouse, replacing the blobs and kzg proofs
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct BlobSidecar {
    #[serde(with = "serde_utils::quoted_u64")]
    pub index: u64,
    pub blob: Blob,
    pub kzg_commitment: LhKzgCommitment,
    pub kzg_proof: KzgProof,
    pub signed_block_header: SignedBeaconBlockHeader,
    pub kzg_commitment_inclusion_proof:
        FixedVector<Hash256, <MainnetEthSpec as EthSpec>::KzgCommitmentInclusionProofDepth>,
}

impl BlobSidecar {
    pub fn new(
        index: usize,
        blob: Blob,
        signed_block: &SignedBeaconBlock,
        kzg_proof: KzgProof,
    ) -> Result<Self, BlobsError> {
        let expected_kzg_commitments = signed_block
            .message()
            .body()
            .blob_kzg_commitments()
            .map_err(|_e| BlobsError::PreDeneb)?;
        let kzg_commitment =
            *expected_kzg_commitments.get(index).ok_or(BlobsError::MissingKzgCommitment(index))?;
        let kzg_commitment_inclusion_proof = signed_block
            .message()
            .body()
            .kzg_commitment_merkle_proof(index)
            .map_err(|_| BlobsError::FailedInclusionProof)?;

        Ok(Self {
            index: index as u64,
            blob,
            kzg_commitment,
            kzg_proof,
            signed_block_header: signed_block.signed_block_header(),
            kzg_commitment_inclusion_proof,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlobsError {
    #[error("block is pre deneb")]
    PreDeneb,

    #[error("missing kzg commitment at index: {0}")]
    MissingKzgCommitment(usize),

    #[error("failed to get kzg commitment inclusion proof")]
    FailedInclusionProof,

    #[error(
        "blobs bundle length mismatch: proofs: {proofs}, commitments: {commitments}, blobs: {blobs}"
    )]
    BundleMismatch { proofs: usize, commitments: usize, blobs: usize },

    #[error("blobs bundle too large: bundle {got}, max: {max}")]
    BundleTooLarge { got: usize, max: usize },
}
