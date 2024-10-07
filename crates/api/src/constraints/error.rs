use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethereum_consensus::{primitives::BlsPublicKey, ssz};
use helix_datastore::error::AuctioneerError;

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

    #[error("datastore error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("no preconfer found for slot: {slot}")]
    NoPreconferFoundForSlot { slot: u64 },

    #[error("can only set constraints for current slot. request slot: {request_slot}, curr slot: {curr_slot}")]
    CanOnlySetConstraintsForCurrentSlot { request_slot: u64, curr_slot: u64 },

    #[error("proposer duty not found. slot: {slot}, validator_index: {validator_index}")]
    ProposerDutyNotFound { slot: u64, validator_index: usize },

    #[error("not preconfer. request public key: {request_public_key:?}, preconfer public key: {preconfer_public_key:?}")]
    NotPreconfer { request_public_key: BlsPublicKey, preconfer_public_key: BlsPublicKey },

    #[error("set constraints sent too late. ns into slot: {ns_into_slot}, cutoff: {cutoff}")]
    SetConstraintsTooLate { ns_into_slot: u64, cutoff: u64 },

    #[error("constraints already set for slot")]
    ConstraintsAlreadySetForSlot,
}

impl IntoResponse for ConstraintsApiError {
    fn into_response(self) -> Response {
        match self {
            ConstraintsApiError::SerdeDecodeError(err) => (StatusCode::BAD_REQUEST, format!("Serde decode error: {err}")).into_response(),
            ConstraintsApiError::IOError(err) => (StatusCode::BAD_REQUEST, format!("IO error: {err}")).into_response(),
            ConstraintsApiError::SszDeserializeError(err) => (StatusCode::BAD_REQUEST, format!("SSZ deserialize error: {err}")).into_response(),
            ConstraintsApiError::HyperError(err) => (StatusCode::BAD_REQUEST, format!("Hyper error: {err}")).into_response(),
            ConstraintsApiError::AxumError(err) => (StatusCode::BAD_REQUEST, format!("Axum error: {err}")).into_response(),
            ConstraintsApiError::RequestForPastSlot { request_slot, head_slot } => {
                (StatusCode::BAD_REQUEST, format!("request for past slot. request slot: {request_slot}, head slot: {head_slot}")).into_response()
            }
            ConstraintsApiError::ValidatorIsNotProposerForRequestedSlot => {
                (StatusCode::BAD_REQUEST, "validator is not proposer for requested slot").into_response()
            }
            ConstraintsApiError::ProposerDutiesNotKnown => (StatusCode::INTERNAL_SERVER_ERROR, "proposer duties not known").into_response(),
            ConstraintsApiError::LockPoisoned => (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned").into_response(),
            ConstraintsApiError::InvalidSignature(err) => (StatusCode::BAD_REQUEST, format!("Invalid signature. {err:?}")).into_response(),
            ConstraintsApiError::InternalServerError => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
            ConstraintsApiError::AuctioneerError(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("Auctioneer error: {err}")).into_response(),
            ConstraintsApiError::NoPreconferFoundForSlot { slot } => {
                (StatusCode::BAD_REQUEST, format!("no preconfer found for slot: {slot}")).into_response()
            }
            ConstraintsApiError::CanOnlySetConstraintsForCurrentSlot { request_slot, curr_slot } => {
                (StatusCode::BAD_REQUEST, format!("can only set constraints for current slot. request slot: {request_slot}, curr slot: {curr_slot}"))
                    .into_response()
            }
            ConstraintsApiError::ProposerDutyNotFound { slot, validator_index } => {
                (StatusCode::BAD_REQUEST, format!("proposer duty not found. slot: {slot}, validator_index: {validator_index}")).into_response()
            }
            ConstraintsApiError::NotPreconfer { request_public_key, preconfer_public_key } => (
                StatusCode::BAD_REQUEST,
                format!("not preconfer. request public key: {request_public_key:?}, preconfer public key: {preconfer_public_key:?}"),
            )
                .into_response(),
            ConstraintsApiError::SetConstraintsTooLate { ns_into_slot, cutoff } => {
                (StatusCode::BAD_REQUEST, format!("set constraints sent too late. ns into slot: {ns_into_slot}, cutoff: {cutoff}")).into_response()
            }
            ConstraintsApiError::ConstraintsAlreadySetForSlot => (StatusCode::BAD_REQUEST, "constraints already set for slot").into_response(),
        }
    }
}
