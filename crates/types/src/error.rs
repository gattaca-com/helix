#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("Invalid signature")]
    InvalidBlsSignature,
}
