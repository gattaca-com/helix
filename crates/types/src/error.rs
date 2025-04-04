#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("Invalid signature")]
    InvalidBlsSignature,
}

pub type SszError = ssz_types::Error;
pub type CryptoError = lh_bls::Error;
