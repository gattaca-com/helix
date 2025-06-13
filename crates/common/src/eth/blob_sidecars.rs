use std::sync::Arc;

use helix_types::{BlobSidecar, BlobSidecarError, BlobSidecars, SszError, VersionedSignedProposal};
use tracing::error;

#[derive(Debug, thiserror::Error)]
pub enum BuildBlobSidecarError {
    #[error("kzg proof mismatch: proofs: {proofs}, blobs: {blobs}")]
    KzgProofMismatch { proofs: usize, blobs: usize },
    #[error("blob sidecar error: {0:?}")]
    BlobSidecarError(BlobSidecarError),
    #[error("Ssz error: {0:?}")]
    SszError(SszError),
}

// TODO: avoid cloning blobs
pub fn blob_sidecars_from_unblinded_payload(
    unblinded_payload: &VersionedSignedProposal,
) -> Result<BlobSidecars, BuildBlobSidecarError> {
    if unblinded_payload.blobs.len() != unblinded_payload.kzg_proofs.len() {
        return Err(BuildBlobSidecarError::KzgProofMismatch {
            proofs: unblinded_payload.kzg_proofs.len(),
            blobs: unblinded_payload.blobs.len(),
        });
    }

    let mut blob_sidecars = Vec::with_capacity(unblinded_payload.blobs.len());

    for (index, blob) in unblinded_payload.blobs.iter().enumerate() {
        // this is safe cause we checked the length of the blobs and proofs
        let kzg_proof = unblinded_payload.kzg_proofs[index];
        let sidecar =
            BlobSidecar::new(index, blob.clone(), &unblinded_payload.signed_block, kzg_proof)
                .map_err(BuildBlobSidecarError::BlobSidecarError)?;
        blob_sidecars.push(Arc::new(sidecar));
    }

    // TODO: check max
    let sidecars =
        BlobSidecars::new(blob_sidecars, 4096).map_err(BuildBlobSidecarError::SszError)?;

    Ok(sidecars)
}
