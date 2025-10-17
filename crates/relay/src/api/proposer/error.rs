use axum::{
    self,
    response::{IntoResponse, Response},
};
use helix_beacon::error::BeaconClientError;
use helix_common::local_cache::AuctioneerError;
use helix_types::{SigError, Slot, SszError};
use hyper::StatusCode;
use thiserror::Error;

use crate::database::error::DatabaseError;

#[derive(Debug, Error)]
pub enum ProposerApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("ToStrError: {0}")]
    ToStrError(#[from] hyper::header::ToStrError),

    #[error("no bid prepared for request")]
    NoBidPrepared,

    #[error("not the expected proposer index. expected {expected}, got {actual}")]
    UnexpectedProposerIndex { expected: u64, actual: u64 },

    #[error("no validators could be registered")]
    NoValidatorsCouldBeRegistered,

    #[error("data for an unexpected fork was provided")]
    InvalidFork,

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("invalid api key")]
    InvalidApiKey,

    #[error("proposer not registered")]
    ProposerNotRegistered,

    #[error("timestamp too early. {timestamp} < {min_timestamp}")]
    TimestampTooEarly { timestamp: u64, min_timestamp: u64 },

    #[error("timestamp too far in the future. {timestamp} > {max_timestamp}")]
    TimestampTooFarInTheFuture { timestamp: u64, max_timestamp: u64 },

    #[error("request for wrong slot. request slot: {request_slot}, bid slot: {bid_slot}")]
    RequestWrongSlot { request_slot: u64, bid_slot: u64 },

    #[error("slot is too new")]
    SlotTooNew,

    #[error("getHeader sent too late. ms into slot: {ms_into_slot}, cutoff: {cutoff}")]
    GetHeaderRequestTooLate { ms_into_slot: u64, cutoff: u64 },

    #[error("sent too late into slot. cutoff: {cutoff}, request time into slot: {request_time}")]
    GetPayloadRequestTooLate { cutoff: u64, request_time: u64 },

    #[error("no execution payload for this request")]
    NoExecutionPayloadFound,

    #[error("beacon-block and payload header mismatch")]
    BlindedBlockAndPayloadHeaderMismatch,

    #[error("unsupported beacon chain version")]
    UnsupportedBeaconChainVersion,

    #[error("beacon client error: {0}")]
    BeaconClientError(#[from] BeaconClientError),

    #[error("database error: {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("auctioneer error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("empty request")]
    EmptyRequest,

    #[error("internal server error")]
    InternalServerError,

    #[error("service unavailable")]
    ServiceUnavailableError,

    #[error("number of blinded blobs does not match blobs bundle length")]
    BlindedBlobsBundleLengthMismatch,

    #[error("internal slot: {internal_slot} does not match slot duty slot: {slot_duty_slot}")]
    InternalSlotMismatchesWithSlotDuty { internal_slot: Slot, slot_duty_slot: Slot },

    #[error(
        "internal slot: {internal_slot} does not match blinded block slot: {blinded_block_slot}"
    )]
    InvalidBlindedBlockSlot { internal_slot: Slot, blinded_block_slot: Slot },

    #[error("blob kzg commitments mismatch in blinded block and payload")]
    BlobKzgCommitmentsMismatch,

    #[error("ssz error: {0:?}")]
    SszError(SszError),

    #[error(transparent)]
    SigError(#[from] SigError),

    #[error("already delivering payload")]
    DeliveringPayload,

    #[error("duplicate payload request")]
    GetPayloadAlreadyReceived,

    #[error("request for past slot. request slot: {request_slot}, head slot: {head_slot}")]
    RequestForPastSlot { request_slot: Slot, head_slot: Slot },

    #[error("{0}")]
    InvalidGetHeader(&'static str),
}

impl IntoResponse for ProposerApiError {
    fn into_response(self) -> Response {
        let code =
            match self {
                ProposerApiError::NoBidPrepared |
                ProposerApiError::GetHeaderRequestTooLate { .. } => StatusCode::NO_CONTENT,

                ProposerApiError::HyperError(_) |
                ProposerApiError::AxumError(_) |
                ProposerApiError::ToStrError(_) |
                ProposerApiError::UnexpectedProposerIndex { .. } |
                ProposerApiError::NoValidatorsCouldBeRegistered |
                ProposerApiError::InvalidFork |
                ProposerApiError::SerdeDecodeError(_) |
                ProposerApiError::ProposerNotRegistered |
                ProposerApiError::TimestampTooEarly { .. } |
                ProposerApiError::TimestampTooFarInTheFuture { .. } |
                ProposerApiError::RequestWrongSlot { .. } |
                ProposerApiError::SlotTooNew |
                ProposerApiError::GetPayloadRequestTooLate { .. } |
                ProposerApiError::BlindedBlockAndPayloadHeaderMismatch |
                ProposerApiError::UnsupportedBeaconChainVersion |
                ProposerApiError::BeaconClientError(_) |
                ProposerApiError::DatabaseError(_) |
                ProposerApiError::AuctioneerError(_) |
                ProposerApiError::EmptyRequest |
                ProposerApiError::BlindedBlobsBundleLengthMismatch |
                ProposerApiError::InternalSlotMismatchesWithSlotDuty { .. } |
                ProposerApiError::InvalidBlindedBlockSlot { .. } |
                ProposerApiError::BlobKzgCommitmentsMismatch |
                ProposerApiError::SszError(_) |
                ProposerApiError::SigError(_) |
                ProposerApiError::DeliveringPayload |
                ProposerApiError::GetPayloadAlreadyReceived |
                ProposerApiError::RequestForPastSlot { .. } => StatusCode::BAD_REQUEST,

                ProposerApiError::InvalidApiKey => StatusCode::UNAUTHORIZED,

                ProposerApiError::InternalServerError |
                ProposerApiError::NoExecutionPayloadFound => StatusCode::INTERNAL_SERVER_ERROR,

                ProposerApiError::ServiceUnavailableError => StatusCode::SERVICE_UNAVAILABLE,

                ProposerApiError::InvalidGetHeader(_) => StatusCode::UNAUTHORIZED,
            };

        (code, self.to_string()).into_response()
    }
}
