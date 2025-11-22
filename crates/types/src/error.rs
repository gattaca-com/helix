#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("invalid signature bytes")]
    InvalidBlsSignatureBytes,

    #[error("invalid pubkey bytes")]
    InvalidBlsPubkeyBytes,

    #[error("invalid signature")]
    InvalidBlsSignature,
}

pub type SszError = ssz_types::Error;
pub type CryptoError = lh_bls::Error;

#[derive(Debug, Clone, thiserror::Error)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sig_error_display() {
        let err = SigError::InvalidBlsSignatureBytes;
        assert_eq!(err.to_string(), "invalid signature bytes");

        let err = SigError::InvalidBlsPubkeyBytes;
        assert_eq!(err.to_string(), "invalid pubkey bytes");

        let err = SigError::InvalidBlsSignature;
        assert_eq!(err.to_string(), "invalid signature");
    }

    #[test]
    fn test_sig_error_debug() {
        let err = SigError::InvalidBlsSignatureBytes;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("InvalidBlsSignatureBytes"));
    }

    #[test]
    fn test_blobs_error_pre_deneb() {
        let err = BlobsError::PreDeneb;
        assert_eq!(err.to_string(), "block is pre deneb");
    }

    #[test]
    fn test_blobs_error_missing_kzg_commitment() {
        let err = BlobsError::MissingKzgCommitment(5);
        assert_eq!(err.to_string(), "missing kzg commitment at index: 5");
    }

    #[test]
    fn test_blobs_error_failed_inclusion_proof() {
        let err = BlobsError::FailedInclusionProof;
        assert_eq!(err.to_string(), "failed to get kzg commitment inclusion proof");
    }

    #[test]
    fn test_blobs_error_bundle_mismatch() {
        let err = BlobsError::BundleMismatch {
            proofs: 3,
            commitments: 4,
            blobs: 5,
        };
        assert_eq!(
            err.to_string(),
            "blobs bundle length mismatch: proofs: 3, commitments: 4, blobs: 5"
        );
    }

    #[test]
    fn test_blobs_error_bundle_too_large() {
        let err = BlobsError::BundleTooLarge { got: 10, max: 6 };
        assert_eq!(err.to_string(), "blobs bundle too large: bundle 10, max: 6");
    }

    #[test]
    fn test_blobs_error_clone() {
        let err1 = BlobsError::PreDeneb;
        let err2 = err1.clone();
        assert_eq!(err1.to_string(), err2.to_string());
    }

    #[test]
    fn test_blobs_error_debug() {
        let err = BlobsError::BundleMismatch {
            proofs: 1,
            commitments: 2,
            blobs: 3,
        };
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("BundleMismatch"));
    }

    #[test]
    fn test_all_sig_error_variants() {
        let errors = vec![
            SigError::InvalidBlsSignatureBytes,
            SigError::InvalidBlsPubkeyBytes,
            SigError::InvalidBlsSignature,
        ];

        // All variants should be creatable
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn test_all_blobs_error_variants() {
        let errors = vec![
            BlobsError::PreDeneb,
            BlobsError::MissingKzgCommitment(0),
            BlobsError::FailedInclusionProof,
            BlobsError::BundleMismatch {
                proofs: 0,
                commitments: 0,
                blobs: 0,
            },
            BlobsError::BundleTooLarge { got: 0, max: 0 },
        ];

        // All variants should be creatable
        assert_eq!(errors.len(), 5);
    }
}
