#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("Invalid signature bytes")]
    InvalidBlsSignatureBytes,

    #[error("Invalid pubkey bytes")]
    InvalidBlsPubkeyBytes,

    #[error("Invalid signature")]
    InvalidBlsSignature,
}

pub type SszError = ssz_types::Error;
pub type CryptoError = lh_bls::Error;

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

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("ssz_error: {0:?}")]
    SszError(SszError),

    #[error(transparent)]
    BlobsError(BlobsError),
}
