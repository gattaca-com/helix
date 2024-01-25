use deadpool_postgres::PoolError;
use ethereum_consensus::primitives::BlsPublicKey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Postgres Pool error: {0}")]
    PostgresPool(#[from] PoolError),

    #[error("PostgresDB error: {0}")]
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

    #[error("Could not find builder info for public key {public_key:?}")]
    BuilderInfoNotFound { public_key: BlsPublicKey },

    #[error("Could not fetch all builder info")]
    AllBuilderInfoNotFound,

    #[error("redis error: {0}")]
    RedisError(String),

    #[error("serde_json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    #[error("from hex error: {0}")]
    FromHexError(#[from] hex::FromHexError),

    #[error("ethereum common error: {0}")]
    EthereumTypesError(#[from] ethereum_consensus::crypto::Error),

    #[error("Block submission already exists")]
    BlockSubmissionAlreadyExists,

    #[error("Block submission not found")]
    RowParsingError(#[from] Box<dyn std::error::Error + Sync + Send>),

    #[error("General error")]
    GeneralError,
}
