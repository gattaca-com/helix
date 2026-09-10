use deadpool_postgres::PoolError;
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, CryptoError, SszError};
use thiserror::Error;

/// `tokio_postgres::Error` displays as just "db error": the SQLSTATE and the server's
/// message live in its source chain.
fn source_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        out.push_str(": ");
        out.push_str(&err.to_string());
        source = err.source();
    }
    out
}

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Postgres Pool error: {}", source_chain(.0))]
    PostgresPool(#[from] PoolError),

    #[error("PostgresDB error: {}", source_chain(.0))]
    Postgres(#[from] tokio_postgres::Error),

    #[error("Validator registration not found")]
    ValidatorRegistrationNotFound,

    #[error("Mismatch between placeholders and parameters.")]
    KnownValidatorsFailedToSet,

    #[error("Proposer duties not found")]
    ProposerDutiesNotFound,

    #[error("Known validators not found")]
    KnownValidatorsNotFound,

    #[error("Validator for public key {public_key:?} not found")]
    ValidatorNotFound { public_key: BlsPublicKey },

    #[error("Could not find builder info for public key {public_key}")]
    BuilderInfoNotFound { public_key: BlsPublicKeyBytes },

    #[error("Could not fetch all builder info")]
    AllBuilderInfoNotFound,

    #[error("Builder info already exists for public key {public_key}")]
    BuilderAlreadyExists { public_key: BlsPublicKeyBytes },

    #[error("serde_json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    #[error("Failed to send to block submission channel")]
    ChannelSendError,

    #[error("Block submission already exists")]
    BlockSubmissionAlreadyExists,

    #[error("Block submission not found")]
    RowParsingError(#[from] Box<dyn std::error::Error + Sync + Send>),

    #[error("SSZ error: {0:?}")]
    SszError(SszError),

    #[error("Crypto error: {0:?}")]
    CryptoError(CryptoError),

    #[error("Invalid bytes")]
    InvalidBlsBytes,

    #[error("General error")]
    GeneralError,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cause must reach the log: a bare Display drops it.
    #[test]
    fn source_chain_joins_every_cause() {
        #[derive(Debug, Error)]
        #[error("inner")]
        struct Inner;

        #[derive(Debug, Error)]
        #[error("outer")]
        struct Outer(#[source] Inner);

        assert_eq!(source_chain(&Outer(Inner)), "outer: inner");
    }
}
