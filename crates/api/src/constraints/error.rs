use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethereum_consensus::crypto::PublicKey;
use helix_common::proofs::ProofError;
use helix_database::error::DatabaseError;
use helix_datastore::error::AuctioneerError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Conflict {
    #[error("Multiple ToB constraints per slot")]
    TopOfBlock,
    #[error("Duplicate transaction in the same slot")]
    DuplicateTransaction,
}

#[derive(Debug, Error)]
pub enum ConstraintsApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("Invalid constraints")]
    InvalidConstraints,

    #[error("Invalid delegation")]
    InvalidDelegation,

    #[error("Invalid revocation")]
    InvalidRevocation,

    #[error("Invalid signature")]
    InvalidSignature,

    #[error("Constraints field is empty")]
    NilConstraints,

    #[error("datastore error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("internal error")]
    InternalError,

    #[error("failed to get constraints proof data")]
    ConstraintsProofDataError(#[from] ProofError),

    #[error(transparent)]
    Conflict(#[from] Conflict),

    #[error("Max constraints per slot reached")]
    MaxConstraintsReached,

    #[error("database error: {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("Missing proposer info")]
    MissingProposerInfo,

    #[error("Pubkey not authorized to submit constraints: {0}")]
    PubkeyNotAuthorized(PublicKey),
}

impl IntoResponse for ConstraintsApiError {
    fn into_response(self) -> Response {
        match self {
            ConstraintsApiError::HyperError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Hyper error: {err}")).into_response()
            }
            ConstraintsApiError::AxumError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Axum error: {err}")).into_response()
            }
            ConstraintsApiError::SerdeDecodeError(err) => {
                (StatusCode::BAD_REQUEST, format!("Serde decode error: {err}")).into_response()
            }
            ConstraintsApiError::InvalidConstraints => {
                (StatusCode::BAD_REQUEST, "Invalid constraints").into_response()
            }
            ConstraintsApiError::InvalidDelegation => {
                (StatusCode::BAD_REQUEST, "Invalid delegation").into_response()
            }
            ConstraintsApiError::InvalidRevocation => {
                (StatusCode::BAD_REQUEST, "Invalid revocation").into_response()
            }
            ConstraintsApiError::InvalidSignature => {
                (StatusCode::BAD_REQUEST, "Invalid signature").into_response()
            }
            ConstraintsApiError::NilConstraints => {
                (StatusCode::BAD_REQUEST, "Constraints field is empty").into_response()
            }
            ConstraintsApiError::AuctioneerError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Auctioneer error: {err}"))
                    .into_response()
            }
            ConstraintsApiError::InternalError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
            ConstraintsApiError::ConstraintsProofDataError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Constraints proof data error: {err}"))
                    .into_response()
            }
            ConstraintsApiError::Conflict(err) => {
                (StatusCode::CONFLICT, format!("Conflict: {err}")).into_response()
            }
            ConstraintsApiError::MaxConstraintsReached => {
                (StatusCode::BAD_REQUEST, "Max constraints per slot reached").into_response()
            }
            ConstraintsApiError::DatabaseError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {err}"))
                    .into_response()
            }
            ConstraintsApiError::MissingProposerInfo => {
                (StatusCode::BAD_REQUEST, "Missing proposer info").into_response()
            }
            ConstraintsApiError::PubkeyNotAuthorized(pubkey) => (
                StatusCode::UNAUTHORIZED,
                format!("Pubkey not authorized to submit constraints: {pubkey}"),
            )
                .into_response(),
        }
    }
}
