use std::sync::Arc;

use alloy_eips::{eip7594::CELLS_PER_EXT_BLOB, eip7691::MAX_BLOBS_PER_BLOCK_ELECTRA};
use lh_types::{test_utils::TestRandom, ForkName, ForkVersionDecode};
use rand::Rng;
use serde::{Deserialize, Serialize};
use ssz::DecodeError;
use ssz_derive::Encode;

use crate::{
    fields::{KzgCommitment, KzgCommitments, KzgProof, KzgProofs},
    BlobsError, ExecutionPayload, SignedBeaconBlock, ValidationError,
};

pub type Blob = Arc<alloy_consensus::Blob>;
pub type Blobs = Vec<Arc<alloy_consensus::Blob>>;

pub enum BlobsBundleVersion {
    V1,
    V2,
}

/// This includes all bundled blob related data of an executed payload.
/// From [`alloy_rpc_types_engine::BlobsBundleV1`]
#[derive(
    Clone, Debug, Default, PartialEq, serde::Serialize, ssz_derive::Encode, ssz_derive::Decode,
)]
pub struct BlobsBundleV1 {
    /// All commitments in the bundle.
    pub commitments: KzgCommitments,
    /// All proofs in the bundle.
    pub proofs: Vec<KzgProof>,
    /// All blobs in the bundle.
    pub blobs: Vec<Arc<alloy_consensus::Blob>>,
}

impl<'de> serde::Deserialize<'de> for BlobsBundleV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct BlobsBundleRaw {
            commitments: KzgCommitments,
            proofs: Vec<KzgProof>,
            blobs: Vec<Arc<alloy_consensus::Blob>>,
        }
        let raw = BlobsBundleRaw::deserialize(deserializer)?;

        let proofs_valid = raw.commitments.len() == raw.proofs.len();
        let blobs_valid = raw.proofs.len() == raw.blobs.len();

        if proofs_valid && blobs_valid {
            Ok(Self { commitments: raw.commitments, proofs: raw.proofs, blobs: raw.blobs })
        } else {
            Err(serde::de::Error::custom(format!(
                "BlobsBundleV1 validation failed: commitments={}, proofs={}, blobs={}",
                raw.commitments.len(),
                raw.proofs.len(),
                raw.blobs.len()
            )))
        }
    }
}

impl TestRandom for BlobsBundleV1 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let n = rng.random_range(0..=MAX_BLOBS_PER_BLOCK_ELECTRA) as usize;

        let mut bundle = Self::with_capacity(n);

        for _ in 0..n {
            bundle.commitments.push(KzgCommitment::random()).unwrap();
            bundle.proofs.push(KzgProof::random());
            bundle.blobs.push(Arc::new(alloy_consensus::Blob::random()));
        }

        bundle
    }
}

impl BlobsBundleV1 {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            commitments: KzgCommitments::new(Vec::with_capacity(capacity)).unwrap(),
            proofs: Vec::with_capacity(capacity),
            blobs: Vec::with_capacity(capacity),
        }
    }

    pub fn validate_ssz_lengths(&self) -> Result<(), ValidationError> {
        if self.commitments.len() != self.proofs.len() || self.proofs.len() != self.blobs.len() {
            return Err(ValidationError::BlobsError(BlobsError::BundleMismatch {
                proofs: self.proofs.len(),
                commitments: self.commitments.len(),
                blobs: self.blobs.len(),
            }));
        }

        if self.commitments.len() > MAX_BLOBS_PER_BLOCK_ELECTRA as usize {
            return Err(ValidationError::BlobsError(BlobsError::BundleTooLarge {
                got: self.commitments.len(),
                max: MAX_BLOBS_PER_BLOCK_ELECTRA as usize,
            }));
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, ssz_derive::Encode)]
pub struct BlobsBundleV2 {
    pub commitments: KzgCommitments,
    pub proofs: KzgProofs,
    pub blobs: Blobs,
}

impl<'de> serde::Deserialize<'de> for BlobsBundleV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct BlobsBundleRaw {
            commitments: KzgCommitments,
            proofs: KzgProofs,
            blobs: Blobs,
        }
        let raw = BlobsBundleRaw::deserialize(deserializer)?;

        let expected_proofs = raw.blobs.len() * CELLS_PER_EXT_BLOB;
        let proofs_valid = raw.proofs.len() == expected_proofs;
        let commitments_valid = raw.commitments.len() == raw.blobs.len();

        if proofs_valid && commitments_valid {
            Ok(Self { commitments: raw.commitments, proofs: raw.proofs, blobs: raw.blobs })
        } else {
            Err(serde::de::Error::custom(format!(
                "BlobsBundleV2 validation failed: expected {} proofs for {} blobs, got {}",
                expected_proofs,
                raw.blobs.len(),
                raw.proofs.len()
            )))
        }
    }
}

impl ssz::Decode for BlobsBundleV2 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct BlobsBundleRaw {
            commitments: KzgCommitments,
            proofs: KzgProofs,
            blobs: Blobs,
        }

        let raw = BlobsBundleRaw::from_ssz_bytes(bytes)?;

        if raw.proofs.len() == raw.blobs.len() * CELLS_PER_EXT_BLOB &&
            raw.commitments.len() == raw.blobs.len()
        {
            Ok(Self { commitments: raw.commitments, proofs: raw.proofs, blobs: raw.blobs })
        } else {
            Err(ssz::DecodeError::BytesInvalid(
                format!(
                    "Invalid BlobsBundleV2: expected {} proofs and {} commitments for {} blobs, got {} proofs and {} commitments",
                    raw.blobs.len() * CELLS_PER_EXT_BLOB,
                    raw.blobs.len(),
                    raw.blobs.len(),
                    raw.proofs.len(),
                    raw.commitments.len()
                )
            ))
        }
    }
}

impl TestRandom for BlobsBundleV2 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let n = rng.random_range(0..=6) as usize;

        let mut bundle = Self::with_capacity(n);

        for _ in 0..n {
            bundle.commitments.push(KzgCommitment::random()).unwrap();
            bundle.proofs.push(KzgProof::random());
            bundle.blobs.push(Arc::new(alloy_consensus::Blob::random()));
        }

        bundle
    }
}

impl BlobsBundleV2 {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            commitments: KzgCommitments::new(Vec::with_capacity(capacity)).unwrap(),
            proofs: Vec::with_capacity(capacity * CELLS_PER_EXT_BLOB),
            blobs: Vec::with_capacity(capacity),
        }
    }

    pub fn validate_ssz_lengths(&self, max_blobs_per_block: usize) -> Result<(), ValidationError> {
        if self.commitments.len() != self.blobs.len() ||
            self.proofs.len() != self.blobs.len() * CELLS_PER_EXT_BLOB
        {
            return Err(ValidationError::BlobsError(BlobsError::BundleMismatch {
                proofs: self.proofs.len(),
                commitments: self.commitments.len(),
                blobs: self.blobs.len(),
            }));
        }

        if self.commitments.len() > max_blobs_per_block {
            return Err(ValidationError::BlobsError(BlobsError::BundleTooLarge {
                got: self.commitments.len(),
                max: max_blobs_per_block,
            }));
        }

        Ok(())
    }
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum BlobsBundle {
    V2(Arc<BlobsBundleV2>),
    V1(Arc<BlobsBundleV1>),
}

impl BlobsBundle {
    pub fn validate_ssz_lengths(&self, max_blobs_per_block: usize) -> Result<(), ValidationError> {
        match self {
            BlobsBundle::V2(bundle) => bundle.validate_ssz_lengths(max_blobs_per_block),
            BlobsBundle::V1(bundle) => bundle.validate_ssz_lengths(),
        }
    }

    pub fn commitments(&self) -> &KzgCommitments {
        match self {
            BlobsBundle::V2(bundle) => &bundle.commitments,
            BlobsBundle::V1(bundle) => &bundle.commitments,
        }
    }

    pub fn proofs(&self) -> &KzgProofs {
        match self {
            BlobsBundle::V2(bundle) => &bundle.proofs,
            BlobsBundle::V1(bundle) => &bundle.proofs,
        }
    }

    pub fn blobs(&self) -> &Blobs {
        match self {
            BlobsBundle::V2(bundle) => &bundle.blobs,
            BlobsBundle::V1(bundle) => &bundle.blobs,
        }
    }

    pub fn iter_blobs(
        &self,
    ) -> Box<dyn Iterator<Item = (&Blob, &KzgCommitment, &[KzgProof])> + '_> {
        match self {
            BlobsBundle::V1(bundle) => {
                // V1: 1 proof per blob
                Box::new(
                    bundle
                        .blobs
                        .iter()
                        .zip(bundle.commitments.iter())
                        .zip(bundle.proofs.iter())
                        .map(|((blob, commitment), proof)| {
                            (blob, commitment, std::slice::from_ref(proof))
                        }),
                )
            }
            BlobsBundle::V2(bundle) => {
                // V2: 128 proofs per blob
                Box::new(bundle.blobs.iter().zip(bundle.commitments.iter()).enumerate().map(
                    |(i, (blob, commitment))| {
                        let start = i * CELLS_PER_EXT_BLOB;
                        let end = start + CELLS_PER_EXT_BLOB;
                        let proofs_slice = &bundle.proofs[start..end];
                        (blob, commitment, proofs_slice)
                    },
                ))
            }
        }
    }

    /// Create a new empty bundle of the specified version
    pub fn new_v1() -> Self {
        BlobsBundle::V1(Arc::new(BlobsBundleV1::default()))
    }

    pub fn new_v2() -> Self {
        BlobsBundle::V2(Arc::new(BlobsBundleV2::default()))
    }

    /// Create with pre-allocated capacity
    pub fn with_capacity_v1(capacity: usize) -> Self {
        BlobsBundle::V1(Arc::new(BlobsBundleV1::with_capacity(capacity)))
    }

    pub fn with_capacity_v2(capacity: usize) -> Self {
        BlobsBundle::V2(Arc::new(BlobsBundleV2::with_capacity(capacity)))
    }

    pub fn version(&self) -> BlobsBundleVersion {
        match self {
            BlobsBundle::V1(_) => BlobsBundleVersion::V1,
            BlobsBundle::V2(_) => BlobsBundleVersion::V2,
        }
    }
}

impl Default for BlobsBundle {
    fn default() -> Self {
        Self::V1(Arc::new(BlobsBundleV1::default()))
    }
}

pub enum BlobsBundleMut {
    V2(BlobsBundleV2),
    V1(BlobsBundleV1),
}

impl BlobsBundleMut {
    pub fn from_bundle(bundle: BlobsBundle) -> Self {
        match bundle {
            BlobsBundle::V1(b) => {
                BlobsBundleMut::V1(Arc::try_unwrap(b).unwrap_or_else(|b| (*b).clone()))
            }
            BlobsBundle::V2(b) => {
                BlobsBundleMut::V2(Arc::try_unwrap(b).unwrap_or_else(|b| (*b).clone()))
            }
        }
    }

    pub fn into_bundle(self) -> BlobsBundle {
        match self {
            BlobsBundleMut::V1(b) => BlobsBundle::V1(Arc::new(b)),
            BlobsBundleMut::V2(b) => BlobsBundle::V2(Arc::new(b)),
        }
    }

    pub fn commitments(&self) -> &KzgCommitments {
        match self {
            BlobsBundleMut::V2(bundle) => &bundle.commitments,
            BlobsBundleMut::V1(bundle) => &bundle.commitments,
        }
    }

    pub fn push_blob(
        &mut self,
        commitment: KzgCommitment,
        proofs: &[KzgProof],
        blob: Blob,
    ) -> Result<(), BlobsError> {
        match self {
            BlobsBundleMut::V2(bundle) => {
                bundle.commitments.push(commitment).map_err(|_| BlobsError::BundleTooLarge {
                    got: bundle.blobs.len() + 1,
                    max: 6,
                })?;
                bundle.proofs.extend_from_slice(proofs);
                bundle.blobs.push(blob);
                Ok(())
            }
            BlobsBundleMut::V1(bundle) => {
                bundle.commitments.push(commitment).map_err(|_| BlobsError::BundleTooLarge {
                    got: bundle.blobs.len() + 1,
                    max: MAX_BLOBS_PER_BLOCK_ELECTRA as usize,
                })?;
                bundle.proofs.extend_from_slice(proofs);
                bundle.blobs.push(blob);
                Ok(())
            }
        }
    }
}

/// Similar to lighthouse but using our BlobsBundleV1
// TODO: arc the fields
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, Encode)]
pub struct PayloadAndBlobs {
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: BlobsBundle,
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

// From lighthouse, replacing the blobs and kzg proofs
#[derive(Debug, Clone, Serialize, Deserialize, Encode)]
pub struct SignedBlockContents {
    pub signed_block: Arc<SignedBeaconBlock>,
    pub kzg_proofs: KzgProofs,
    pub blobs: Blobs,
}

#[cfg(test)]
mod tests {
    use lh_eth2::types::BlobsBundle as LhBlobsBundle;
    use lh_types::{
        test_utils::{TestRandom, XorShiftRng},
        KzgCommitment as LhKzgCommitment, KzgProof as LhKzgProof, MainnetEthSpec,
    };
    use rand::SeedableRng;
    use ssz::Encode;

    use super::*;
    use crate::{test_encode_decode_json, SignedBidSubmission};

    #[test]
    fn test_blobs_bundle_() {
        let mut rng = XorShiftRng::from_seed([42; 16]);

        let our_bundle = BlobsBundleV1::random_for_test(&mut rng);

        let mut lh_bundle = LhBlobsBundle::<MainnetEthSpec>::default();

        for c in our_bundle.commitments.iter() {
            lh_bundle.commitments.push(LhKzgCommitment(c.0)).unwrap();
        }

        for p in &our_bundle.proofs {
            lh_bundle.proofs.push(LhKzgProof(p.0)).unwrap();
        }

        for b in &our_bundle.blobs {
            let blob_bytes: Vec<u8> = b.as_ref().to_vec();
            lh_bundle.blobs.push(blob_bytes.into()).unwrap();
        }

        assert_eq!(our_bundle.as_ssz_bytes(), lh_bundle.as_ssz_bytes());
    }

    #[test]
    fn test_payload_and_blobs_equivalence() {
        let data_json = include_str!("testdata/signed-bid-submission-electra.json");
        let signed_bid = test_encode_decode_json::<SignedBidSubmission>(data_json);
        let ex = signed_bid.payload_and_blobs();

        let data_ssz = ex.as_ssz_bytes();

        let ex_test =
            PayloadAndBlobs::from_ssz_bytes_by_fork(&data_ssz, ForkName::Electra).unwrap();

        assert_eq!(ex, ex_test)
    }
}
