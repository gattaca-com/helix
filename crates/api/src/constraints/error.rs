use std::sync::PoisonError;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethereum_consensus::ssz;
use tokio::sync::RwLockReadGuard;

#[derive(Debug, thiserror::Error)]
pub enum ConstraintsApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("ssz deserialize error: {0}")]
    SszDeserializeError(#[from] ssz::prelude::DeserializeError),

    #[error("request for past slot. request slot: {request_slot}, head slot: {head_slot}")]
    RequestForPastSlot { request_slot: u64, head_slot: u64 },

    #[error("cannot elect gateway too far in the future. request slot: {request_slot}, max slot: {max_slot}")]
    CannotElectGatewayTooFarInTheFuture { request_slot: u64, max_slot: u64 },

    #[error("validator is not proposer for requested slot")]
    ValidatorIsNotProposerForRequestedSlot,

    #[error("proposer duties not known")]
    ProposerDutiesNotKnown,

    #[error("lock poisoned")]
    LockPoisoned,

    #[error("invalid signature: {0}")]
    InvalidSignature(#[from] ethereum_consensus::Error),

    #[error("internal server error")]
    InternalServerError,
}

impl IntoResponse for ConstraintsApiError {
    fn into_response(self) -> Response {
        match self {
            ConstraintsApiError::SerdeDecodeError(err) => {
                (StatusCode::BAD_REQUEST, format!("Serde decode error: {err}")).into_response()
            },
            ConstraintsApiError::IOError(err) => {
                (StatusCode::BAD_REQUEST, format!("IO error: {err}")).into_response()
            },
            ConstraintsApiError::SszDeserializeError(err) => {
                (StatusCode::BAD_REQUEST, format!("SSZ deserialize error: {err}")).into_response()
            },
            ConstraintsApiError::HyperError(err) => {
                (StatusCode::BAD_REQUEST, format!("Hyper error: {err}")).into_response()
            },
            ConstraintsApiError::AxumError(err) => {
                (StatusCode::BAD_REQUEST, format!("Axum error: {err}")).into_response()
            },
            ConstraintsApiError::RequestForPastSlot{request_slot, head_slot} => {
                (StatusCode::BAD_REQUEST, format!("request for past slot. request slot: {request_slot}, head slot: {head_slot}")).into_response()
            },
            ConstraintsApiError::CannotElectGatewayTooFarInTheFuture{request_slot, max_slot} => {
                (StatusCode::BAD_REQUEST, format!("cannot elect gateway too far in the future. request slot: {request_slot}, max slot: {max_slot}")).into_response()
            },
            ConstraintsApiError::ValidatorIsNotProposerForRequestedSlot => {
                (StatusCode::BAD_REQUEST, "validator is not proposer for requested slot").into_response()
            },
            ConstraintsApiError::ProposerDutiesNotKnown => {
                (StatusCode::INTERNAL_SERVER_ERROR, "proposer duties not known").into_response()
            },
            ConstraintsApiError::LockPoisoned => {
                (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned").into_response()
            },
            ConstraintsApiError::InvalidSignature(err) => {
                (StatusCode::BAD_REQUEST, format!("Invalid signature. {err:?}")).into_response()
            },
            ConstraintsApiError::InternalServerError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            },
        }
    }
}
