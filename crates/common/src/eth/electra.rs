use std::sync::Arc;

use alloy_primitives::B256;
use ethereum_consensus::{
    altair::{BeaconBlockHeader, SignedBeaconBlockHeader},
    crypto::{KzgCommitment, KzgProof},
    deneb::mainnet::MAX_BLOBS_PER_BLOCK,
    electra::{
        self,
        mainnet::{
            BlobSidecar, MAX_BLOB_COMMITMENTS_PER_BLOCK, MAX_CONSOLIDATION_REQUESTS_PER_PAYLOAD,
            MAX_DEPOSIT_REQUESTS_PER_PAYLOAD, MAX_WITHDRAWAL_REQUESTS_PER_PAYLOAD,
        },
    },
    primitives::{BlsPublicKey, BlsSignature, Root, U256},
    serde::as_str,
    ssz::prelude::*,
};
pub use ethereum_consensus::{builder::SignedValidatorRegistration, electra::mainnet as spec};
use merkle_proof::MerkleTreeError;
use ssz_types::{typenum::U17, FixedVector};
use thiserror::Error;
use tracing::error;

use crate::{signed_proposal::VersionedSignedProposal, BLOB_KZG_COMMITMENTS_INDEX};

pub type ExecutionRequests = spec::ExecutionRequests<
    MAX_DEPOSIT_REQUESTS_PER_PAYLOAD,
    MAX_WITHDRAWAL_REQUESTS_PER_PAYLOAD,
    MAX_CONSOLIDATION_REQUESTS_PER_PAYLOAD,
>;
pub type ExecutionPayload = spec::ExecutionPayload;
pub type ExecutionPayloadHeader = spec::ExecutionPayloadHeader;
pub type SignedBlindedBeaconBlock = spec::SignedBlindedBeaconBlock;
pub type SignedBlindedBlobSidecar = spec::BlobSidecar;
pub type BeaconBlockBody = spec::BeaconBlockBody;
pub type SignedBeaconBlock = spec::SignedBeaconBlock;
pub type Blob = spec::Blob;

#[derive(
    Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize, HashTreeRoot,
)]
pub struct BuilderBid {
    pub header: spec::ExecutionPayloadHeader,
    pub blob_kzg_commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub execution_requests: ExecutionRequests,
    #[serde(with = "as_str")]
    pub value: U256,
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
}

#[derive(Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize)]
pub struct BlindedBlobsBundle {
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blob_roots: List<Root, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize)]
pub struct SignedBuilderBid {
    pub message: BuilderBid,
    pub signature: BlsSignature,
}

#[derive(Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize)]
pub struct SignedBlindedBlockAndBlobSidecars {
    pub signed_blinded_block: SignedBlindedBeaconBlock,
    pub signed_blinded_blob_sidecars: List<SignedBlindedBlobSidecar, MAX_BLOBS_PER_BLOCK>,
}

#[derive(Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize)]
pub struct BlobsBundle {
    pub commitments: List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SignedBlockContents {
    pub signed_block: SignedBeaconBlock,
    pub kzg_proofs: List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
    pub blobs: List<Blob, MAX_BLOB_COMMITMENTS_PER_BLOCK>,
}

#[derive(
    Debug, Default, Clone, Serializable, serde::Serialize, serde::Deserialize, PartialEq, Eq,
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
            VersionedSignedProposal::Electra(payload) => {
                if payload.blobs.is_empty() {
                    return Err(BuildBlobSidecarError::NoBlobsInPayload);
                }
                payload
            }
            _ => return Err(BuildBlobSidecarError::PayloadVersionBeforeBlobs),
        };

        let mut beacon_block = payload.signed_block.clone();
        let signed_block = beacon_block.electra_mut().unwrap();

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
    signed_block: &mut electra::mainnet::SignedBeaconBlock,
    body_root: Root,
    kzg_proof: KzgProof,
) -> Result<BlobSidecar, BuildBlobSidecarError> {
    let kzg_commitments = &signed_block.message.body.blob_kzg_commitments;
    let kzg_commitment =
        kzg_commitments.get(index).ok_or(BuildBlobSidecarError::MissingKzgCommitment)?.clone();

    let kzg_commitment_inclusion_proof = kzg_commitment_merkle_proof(signed_block, index)?;
    let kzg_commitment_inclusion_proof: Vec<Node> =
        kzg_commitment_inclusion_proof.into_iter().map(|x| Node::from(x.0)).collect();
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
    signed_block: &mut electra::mainnet::SignedBeaconBlock,
    index: usize,
) -> Result<FixedVector<B256, U17>, BuildBlobSidecarError> {
    // We compute the branches by generating 2 merkle trees:
    // 1. Merkle tree for the `blob_kzg_commitments` List object
    // 2. Merkle tree for the `BeaconBlockBody` container
    // We then merge the branches for both the trees all the way up to the root.

    // Part1 (Branches for the subtree rooted at `blob_kzg_commitments`)
    //
    // Branches for `blob_kzg_commitments` without length mix-in
    let mut leaves: Vec<B256> =
        Vec::with_capacity(signed_block.message.body.blob_kzg_commitments.len());
    for commitment in signed_block.message.body.blob_kzg_commitments.iter_mut() {
        let root = commitment.hash_tree_root()?;
        leaves.push(B256::from_slice(root.as_slice()));
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
    let length_root = B256::from_slice(length_bytes.as_slice());
    proof.push(length_root);

    // Part 2
    // Branches for `BeaconBlockBody` container
    let leaves: [B256; 12] = [
        B256::from_slice(signed_block.message.body.randao_reveal.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.eth1_data.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.graffiti.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.proposer_slashings.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.attester_slashings.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.attestations.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.deposits.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.voluntary_exits.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.sync_aggregate.hash_tree_root()?.as_slice()),
        B256::from_slice(signed_block.message.body.execution_payload.hash_tree_root()?.as_slice()),
        B256::from_slice(
            signed_block.message.body.bls_to_execution_changes.hash_tree_root()?.as_slice(),
        ),
        B256::from_slice(
            signed_block.message.body.blob_kzg_commitments.hash_tree_root()?.as_slice(),
        ),
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

#[cfg(test)]
mod tests {
    use ethereum_consensus::{
        ssz::prelude::*,
        types::{mainnet::SignedBlindedBeaconBlock, BlindedBeaconBlockRef},
    };

    use crate::{
        bid_submission::SignedBidSubmissionElectra,
        utils::test_utils::{test_encode_decode_json, test_encode_decode_ssz},
        SignedBuilderBid,
    };

    #[test]
    fn test_signed_builder_electra() {
        let json = r#"{
            "version": "electra",
            "data": {
                "message": {
                    "header": {
                        "parent_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                        "state_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "receipts_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                        "prev_randao": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "block_number": "1",
                        "gas_limit": "1",
                        "gas_used": "1",
                        "timestamp": "1",
                        "extra_data": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "base_fee_per_gas": "1",
                        "blob_gas_used": "1",
                        "excess_blob_gas": "1",
                        "block_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "transactions_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "withdrawals_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"
                    },
                    "blob_kzg_commitments": [
                        "0xa94170080872584e54a1cf092d845703b13907f2e6b3b1c0ad573b910530499e3bcd48c6378846b80d2bfa58c81cf3d5"
                    ],
                    "execution_requests": {
                        "deposits": [
                            {
                                "pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a",
                                "withdrawal_credentials": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                                "amount": "1",
                                "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505",
                                "index": "1"
                            }
                        ],
                        "withdrawals": [
                            {
                                "source_address": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                                "validator_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a",
                                "amount": "1"
                            }
                        ],
                        "consolidations": [
                            {
                                "source_address": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                                "source_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a",
                                "target_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
                            }
                        ]
                    },
                    "value": "1",
                    "pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
                },
                "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
            }
        }"#;

        let signed_builder_bid =
            crate::utils::test_utils::test_encode_decode_json::<SignedBuilderBid>(json);

        assert_eq!(signed_builder_bid.version(), "electra");
        assert!(matches!(signed_builder_bid, SignedBuilderBid::Electra(_, None)));
    }

    #[test]
    // this is from mev-boost test data
    fn test_signed_builder_bid_2() {
        let data_json = include_str!("testdata/signed-builder-bid-electra.json");
        test_encode_decode_json::<super::SignedBuilderBid>(&data_json);
    }

    #[test]
    // this is from mev-boost test data
    fn test_signed_blinded_block_fb() {
        let data_json = include_str!("testdata/signed-blinded-beacon-block-electra.json");
        let block_json = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data_json);
        assert!(matches!(block_json.message(), BlindedBeaconBlockRef::Electra(_)));
    }

    #[test]
    // this is dummy data generated with https://github.com/attestantio/go-eth2-client
    fn test_signed_blinded_beacon_block() {
        let data_json = include_str!("testdata/signed-blinded-beacon-block-electra-2.json");
        let block_json = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data_json);
        assert!(matches!(block_json.message(), BlindedBeaconBlockRef::Electra(_)));

        let mut encoded = Vec::new();
        let written = block_json.serialize(&mut encoded).unwrap();
        assert!(written > 0);

        let data_ssz = include_bytes!("testdata/signed-blinded-beacon-block-electra-2.ssz");
        let data_ssz = alloy_primitives::hex::decode(data_ssz).unwrap();

        assert_eq!(encoded, data_ssz);
        let block_ssz = test_encode_decode_ssz::<SignedBlindedBeaconBlock>(&data_ssz);
        assert!(matches!(block_ssz.message(), BlindedBeaconBlockRef::Electra(_)));

        let mut encoded = Vec::new();
        let written = block_json.serialize(&mut encoded).unwrap();
        assert!(written > 0);

        assert_eq!(encoded, data_ssz);
    }

    #[test]
    // this is from the relay API spec, addging the blob and the proposer_pubkey field
    fn test_submit_block() {
        let data_json = include_str!("testdata/signed-bid-submission-electra.json");
        test_encode_decode_json::<SignedBidSubmissionElectra>(&data_json);
    }

    #[test]
    // this is random data
    fn test_submit_block_2() {
        let data_ssz = include_bytes!("testdata/signed-bid-submission-electra-2.ssz");
        let data_ssz = alloy_primitives::hex::decode(data_ssz).unwrap();
        test_encode_decode_ssz::<SignedBidSubmissionElectra>(&data_ssz);
    }
}
