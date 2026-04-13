use axum::response::{IntoResponse, Response};
use helix_common::{decoder::DecoderError, local_cache::AuctioneerError, simulator::BlockSimError};
use helix_database::error::DatabaseError;
use helix_types::{BlockValidationError, BlsPublicKeyBytes, HydrationError, SigError};
use http::StatusCode;

use crate::auctioneer::OrderValidationError;

#[derive(Debug, thiserror::Error)]
pub enum BuilderApiError {
    #[error("json decode error: {0}")]
    JsonDecodeError(#[from] serde_json::Error),

    #[error("ssz decode error: {0:?}")]
    SszDecode(ssz::DecodeError),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("failed to decode payload")]
    PayloadDecode(#[from] DecoderError),

    #[error("block validation: {0}")]
    BidValidation(#[from] BlockValidationError),

    #[error("signature: {0:?}")]
    SigError(#[from] SigError),

    #[error("block simulation: {0:?}")]
    BlockSimulation(#[from] BlockSimError),

    #[error("hydration: {0:?}")]
    HydrationError(#[from] HydrationError),

    #[error("untrusted builder on dehydrated payload")]
    UntrustedBuilderOnDehydratedPayload,

    #[error("could not find proposer duty for slot")]
    ProposerDutyNotFound,

    #[error("late sim, already on next slot")]
    SimOnNextSlot,

    #[error("delivering payload: bid_slot: {bid_slot}, delivering: {delivering}")]
    DeliveringPayload { bid_slot: u64, delivering: u64 },

    #[error("invalid api key")]
    InvalidApiKey,

    #[error("request timeout")]
    RequestTimeout,

    #[error("datastore error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("could not find mergeable orders: {0}")]
    MergeableOrdersNotFound(#[from] OrderValidationError),

    #[error("database error: {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("internal error")]
    InternalError,

    #[error("submission pubkey {0} not in registered pubkeys")]
    InvalidBuilderPubkey(BlsPublicKeyBytes),
}

impl IntoResponse for BuilderApiError {
    fn into_response(self) -> Response {
        (&self).into_response()
    }
}

impl IntoResponse for &BuilderApiError {
    fn into_response(self) -> Response {
        (self.http_status(), self.to_string()).into_response()
    }
}

impl BuilderApiError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            BuilderApiError::JsonDecodeError(_) |
            BuilderApiError::IOError(_) |
            BuilderApiError::SszDecode(_) |
            BuilderApiError::PayloadDecode(_) |
            BuilderApiError::BidValidation(_) |
            BuilderApiError::ProposerDutyNotFound |
            BuilderApiError::HydrationError(_) |
            BuilderApiError::SigError(_) |
            BuilderApiError::SimOnNextSlot |
            BuilderApiError::MergeableOrdersNotFound(_) |
            BuilderApiError::InvalidBuilderPubkey(_) |
            BuilderApiError::DeliveringPayload { .. } => StatusCode::BAD_REQUEST,

            BuilderApiError::InvalidApiKey |
            BuilderApiError::UntrustedBuilderOnDehydratedPayload => StatusCode::UNAUTHORIZED,

            BuilderApiError::InternalError |
            BuilderApiError::AuctioneerError(_) |
            BuilderApiError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,

            BuilderApiError::BlockSimulation(err) => match err {
                BlockSimError::Timeout | BlockSimError::SimulationDropped => {
                    StatusCode::REQUEST_TIMEOUT
                }
                _ => StatusCode::BAD_REQUEST,
            },

            BuilderApiError::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
        }
    }
}

impl BuilderApiError {
    // when adding new errors to ignore make sure to be very conservative, ie better to log a bit
    // more than to risk not logging a relevant error
    #[allow(clippy::match_like_matches_macro)]
    pub fn should_report(&self) -> bool {
        match self {
            Self::DeliveringPayload { .. } |
            Self::ProposerDutyNotFound |
            Self::BidValidation(BlockValidationError::OutOfSequence { .. }) |
            Self::BidValidation(BlockValidationError::AlreadyProcessingNewerPayload) |
            Self::BidValidation(BlockValidationError::SubmissionForWrongSlot { .. }) |
            Self::BidValidation(BlockValidationError::PrevRandaoMismatch { .. }) |
            Self::SimOnNextSlot => false,

            _ => true,
        }
    }
}
