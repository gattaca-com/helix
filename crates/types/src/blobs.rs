use std::sync::Arc;

use alloy_eips::eip7691::MAX_BLOBS_PER_BLOCK_ELECTRA;
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
pub type Blobs = Vec<Blob>;

/// This includes all bundled blob related data of an executed payload.
/// From [`alloy_rpc_types_engine::BlobsBundleV1`]
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
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

/// Similar to lighthouse but using our BlobsBundleV1
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, Encode)]
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

#[derive(Clone, PartialEq, Debug, Serialize, Encode)]
pub struct PayloadAndBlobsRef<'a> {
    pub execution_payload: &'a ExecutionPayload,
    pub blobs_bundle: &'a BlobsBundleV1,
}

impl<'a> From<&'a PayloadAndBlobs> for PayloadAndBlobsRef<'a> {
    fn from(payload_and_blobs: &'a PayloadAndBlobs) -> Self {
        PayloadAndBlobsRef {
            execution_payload: &payload_and_blobs.execution_payload,
            blobs_bundle: &payload_and_blobs.blobs_bundle,
        }
    }
}

impl PayloadAndBlobsRef<'_> {
    /// Clone out an owned `PayloadAndBlobs`
    pub fn to_owned(&self) -> PayloadAndBlobs {
        let execution_payload = self.execution_payload.clone();
        let blobs_bundle = self.blobs_bundle.clone();
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
        let ex = signed_bid.payload_and_blobs_ref().to_owned();

        let data_ssz = ex.as_ssz_bytes();

        let ex_test =
            PayloadAndBlobs::from_ssz_bytes_by_fork(&data_ssz, ForkName::Electra).unwrap();

        assert_eq!(ex, ex_test)
    }
}
