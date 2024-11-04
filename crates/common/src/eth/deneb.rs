pub use ethereum_consensus::{builder::SignedValidatorRegistration, deneb::mainnet as spec};
use std::sync::Arc;

use crate::{signed_proposal::VersionedSignedProposal, BLOB_KZG_COMMITMENTS_INDEX};
use ethereum_consensus::{
    altair::{BeaconBlockHeader, Bytes32, SignedBeaconBlockHeader},
    deneb,
    deneb::{
        mainnet::{BlobSidecar, MAX_BLOBS_PER_BLOCK, MAX_BLOB_COMMITMENTS_PER_BLOCK},
        polynomial_commitments::{KzgCommitment, KzgProof},
    },
    primitives::{BlsPublicKey, BlsSignature, Root, U256},
    serde::as_str,
    ssz::prelude::*,
    types::mainnet::SignedBeaconBlock,
};
use ethereum_types::H256;
use merkle_proof::MerkleTreeError;
use ssz_types::{typenum::U17, FixedVector};
use thiserror::Error;
use tracing::error;

pub type ExecutionPayload = spec::ExecutionPayload;
pub type ExecutionPayloadHeader = spec::ExecutionPayloadHeader;
pub type SignedBlindedBeaconBlock = spec::SignedBlindedBeaconBlock;
pub type SignedBlindedBlobSidecar = spec::SignedBlindedBlobSidecar;
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
pub struct SignedBlindedBlockAndBlobSidecars {
    pub signed_blinded_block: SignedBlindedBeaconBlock,
    pub signed_blinded_blob_sidecars: List<SignedBlindedBlobSidecar, MAX_BLOBS_PER_BLOCK>,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BlobsBundle {
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SignedBlockContents {
    pub signed_block: SignedBeaconBlock,
    pub kzg_proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(
    Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize, PartialEq, Eq,
)]
pub struct BlobSidecars {
    pub sidecars: List<BlobSidecar, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Error)]
pub enum BuildBlobSidecarError {
    #[error("no blobs in payload")]
    NoBlobsInPayload,
    #[error("payload version before blobs")]
    PayloadVersionBeforeBlobs,
    #[error("missing kzg commitment")]
    MissingKzgCommitment,
    #[error("missing kzg proof")]
    MissingKzgProof,
    #[error("merkle tree error: {0:?}")]
    MerkleTreeError(MerkleTreeError),
    #[error("merkleization error: {0}")]
    MerkleizationError(#[from] MerkleizationError),
    #[error("failed to format inclusion proof")]
    FailedToFormatInclusionProof,
}

impl BlobSidecars {
    pub fn try_from_unblinded_payload(
        unblinded_payload: Arc<VersionedSignedProposal>,
    ) -> Result<Self, BuildBlobSidecarError> {
        let payload = match unblinded_payload.as_ref() {
            VersionedSignedProposal::Deneb(payload) => {
                if payload.blobs.is_empty() {
                    return Err(BuildBlobSidecarError::NoBlobsInPayload)
                }
                payload
            }
            _ => return Err(BuildBlobSidecarError::PayloadVersionBeforeBlobs),
        };

        let mut beacon_block = payload.signed_block.clone();
        let signed_block = beacon_block.deneb_mut().unwrap();

        let body_root = signed_block.message.body.hash_tree_root()?;

        let mut blob_sidecars = Self { sidecars: List::default() };

        for (index, blob) in payload.blobs.iter().enumerate() {
            let kzg_proof = payload
                .kzg_proofs
                .get(index)
                .ok_or(BuildBlobSidecarError::MissingKzgProof)?
                .clone();

            let sidecar =
                new_blob_sidecar(index, blob.clone(), signed_block, body_root, kzg_proof)?;
            blob_sidecars.sidecars.push(sidecar);
        }

        Ok(blob_sidecars)
    }
}

/// Creates a new [BlobSidecar] from a beacon block and `blob` at `index`.
pub fn new_blob_sidecar(
    index: usize,
    blob: Blob,
    signed_block: &mut deneb::mainnet::SignedBeaconBlock,
    body_root: Root,
    kzg_proof: KzgProof,
) -> Result<BlobSidecar, BuildBlobSidecarError> {
    let kzg_commitments = &signed_block.message.body.blob_kzg_commitments;
    let kzg_commitment =
        kzg_commitments.get(index).ok_or(BuildBlobSidecarError::MissingKzgCommitment)?.clone();

    let kzg_commitment_inclusion_proof = kzg_commitment_merkle_proof(signed_block, index)?;
    let kzg_commitment_inclusion_proof: Vec<Bytes32> = kzg_commitment_inclusion_proof
        .into_iter()
        .map(|x| Bytes32::try_from(x.as_bytes()).unwrap())
        .collect();
    let kzg_commitment_inclusion_proof = kzg_commitment_inclusion_proof
        .try_into()
        .map_err(|_| BuildBlobSidecarError::FailedToFormatInclusionProof)?;

    let signed_block_header = SignedBeaconBlockHeader {
        message: BeaconBlockHeader {
            slot: signed_block.message.slot,
            proposer_index: signed_block.message.proposer_index,
            parent_root: signed_block.message.parent_root,
            state_root: signed_block.message.state_root,
            body_root,
        },
        signature: signed_block.signature.clone(),
    };

    Ok(BlobSidecar {
        index,
        blob,
        kzg_commitment,
        kzg_proof,
        signed_block_header,
        kzg_commitment_inclusion_proof,
    })
}

/// Produces the proof of inclusion for a `KzgCommitment` in `self.blob_kzg_commitments`
/// at `index`.
///
/// Taken from Lighthouse.
fn kzg_commitment_merkle_proof(
    signed_block: &mut deneb::mainnet::SignedBeaconBlock,
    index: usize,
) -> Result<FixedVector<H256, U17>, BuildBlobSidecarError> {
    // We compute the branches by generating 2 merkle trees:
    // 1. Merkle tree for the `blob_kzg_commitments` List object
    // 2. Merkle tree for the `BeaconBlockBody` container
    // We then merge the branches for both the trees all the way up to the root.

    // Part1 (Branches for the subtree rooted at `blob_kzg_commitments`)
    //
    // Branches for `blob_kzg_commitments` without length mix-in
    let mut leaves: Vec<H256> =
        Vec::with_capacity(signed_block.message.body.blob_kzg_commitments.len());
    for commitment in signed_block.message.body.blob_kzg_commitments.iter_mut() {
        let root = commitment.hash_tree_root()?;
        leaves.push(H256::from_slice(&root));
    }

    let depth = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two().ilog2() as usize;

    let tree = merkle_proof::MerkleTree::create(&leaves, depth);
    let (_, mut proof) =
        tree.generate_proof(index, depth).map_err(BuildBlobSidecarError::MerkleTreeError)?;

    // Add the branch corresponding to the length mix-in.
    let length = signed_block.message.body.blob_kzg_commitments.len();
    let mut length_bytes = [0; 32];

    length_bytes
        .get_mut(0..std::mem::size_of::<usize>())
        .unwrap()
        .copy_from_slice(&length.to_le_bytes());
    let length_root = H256::from_slice(length_bytes.as_slice());
    proof.push(length_root);

    // Part 2
    // Branches for `BeaconBlockBody` container
    let leaves: [H256; 12] = [
        H256::from_slice(&signed_block.message.body.randao_reveal.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.eth1_data.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.graffiti.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.proposer_slashings.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.attester_slashings.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.attestations.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.deposits.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.voluntary_exits.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.sync_aggregate.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.execution_payload.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.bls_to_execution_changes.hash_tree_root()?),
        H256::from_slice(&signed_block.message.body.blob_kzg_commitments.hash_tree_root()?),
    ];
    let beacon_block_body_depth = leaves.len().next_power_of_two().ilog2() as usize;
    let tree = merkle_proof::MerkleTree::create(&leaves, beacon_block_body_depth);
    let (_, mut proof_body) = tree
        .generate_proof(BLOB_KZG_COMMITMENTS_INDEX, beacon_block_body_depth)
        .map_err(BuildBlobSidecarError::MerkleTreeError)?;
    // Join the proofs for the subtree and the main tree
    proof.append(&mut proof_body);

    Ok(proof.into())
}
