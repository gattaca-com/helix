use alloy_primitives::{Address, B256};
use axum::{
    self,
    response::{IntoResponse, Response},
};
use helix_beacon::error::BeaconClientError;
use helix_common::local_cache::AuctioneerError;
use helix_database::error::DatabaseError;
use helix_types::{BlsPublicKey, SigError, Slot, SszError};
use hyper::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProposerApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("ToStrError: {0}")]
    ToStrError(#[from] hyper::header::ToStrError),

    #[error("bid public key {bid:?} does not match relay public key {relay:?}")]
    BidPublicKeyMismatch { bid: Box<BlsPublicKey>, relay: Box<BlsPublicKey> },

    #[error("no bid prepared for request")]
    NoBidPrepared,

    #[error("no valid bids returned for proposal")]
    NoBids,

    #[error("bid has value 0")]
    BidValueZero,

    #[error("could not find relay with outstanding bid to accept")]
    MissingOpenBid,

    #[error("could not find proposer for slot {0:?}")]
    MissingProposer(Slot),

    #[error("not the expected proposer index. expected {expected}, got {actual}")]
    UnexpectedProposerIndex { expected: u64, actual: u64 },

    #[error("no validators could be registered")]
    NoValidatorsCouldBeRegistered,

    #[error("no preferences found for validator with public key {0:?}")]
    MissingPreferences(BlsPublicKey),

    #[error("no payload returned for opened bid with block hash {0:?}")]
    MissingPayload(B256),

    #[error("payload gas limit does not match the proposer's preference")]
    InvalidGasLimit,

    #[error("data for an unexpected fork was provided")]
    InvalidFork,

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("block does not match the provided header")]
    UnknownBlock,

    #[error("payload request does not match any outstanding bid")]
    UnknownBid,

    #[error("validator {0} does not have {1} fee recipient")]
    UnknownFeeRecipient(BlsPublicKey, Address),

    #[error("validator with public key {0:?} is not currently registered")]
    ValidatorNotRegistered(BlsPublicKey),

    #[error("validator with public key {0:?} is unknown")]
    UnknownValidator(BlsPublicKey),

    #[error("invalid signature")]
    InvalidSignature,

    #[error("invalid api key")]
    InvalidApiKey,

    #[error("proposer not registered")]
    ProposerNotRegistered,

    #[error("timestamp too early. {timestamp} < {min_timestamp}")]
    TimestampTooEarly { timestamp: u64, min_timestamp: u64 },

    #[error("timestamp too far in the future. {timestamp} > {max_timestamp}")]
    TimestampTooFarInTheFuture { timestamp: u64, max_timestamp: u64 },

    #[error("no previous registration timestamp")]
    NoPreviousRegistrationTimestamp,

    #[error("registration timestamp too old")]
    RegistrationTimestampTooOld,

    #[error("request for wrong slot. request slot: {request_slot}, bid slot: {bid_slot}")]
    RequestWrongSlot { request_slot: u64, bid_slot: u64 },

    #[error("slot is too new")]
    SlotTooNew,

    #[error("getHeader sent too late. ms into slot: {ms_into_slot}, cutoff: {cutoff}")]
    GetHeaderRequestTooLate { ms_into_slot: u64, cutoff: u64 },

    #[error("could not verify payload signature")]
    InvalidPayloadSignature,

    #[error("sent too late into slot. cutoff: {cutoff}, request time into slot: {request_time}")]
    GetPayloadRequestTooLate { cutoff: u64, request_time: u64 },

    #[error("no execution payload for this request")]
    NoExecutionPayloadFound,

    #[error("could not verify payload signature")]
    InvalidExecutionPayloadSignature,

    #[error("invalid beacon chain version")]
    InvalidBeaconChainVersion,

    #[error("payload type mismatch")]
    PayloadTypeMismatch,

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

    #[error("expected parent hash: {expected_parent_hash:?} does not match blinded block parent hash: {blinded_block_parent_hash:?}")]
    InvalidBlindedBlockParentHash { expected_parent_hash: B256, blinded_block_parent_hash: B256 },

    #[error("parent hash unknown for slot: {slot}")]
    ParentHashUnknownForSlot { slot: u64 },

    #[error("get header disabled for proposer")]
    GetHeaderDisabledForProposer,

    #[error("not serving headers")]
    NotServingHeaders,

    #[error("blob kzg commitments mismatch in blinded block and payload")]
    BlobKzgCommitmentsMismatch,

    #[error("ssz error: {0:?}")]
    SszError(SszError),

    #[error(transparent)]
    SigError(#[from] SigError),
}

impl IntoResponse for ProposerApiError {
    fn into_response(self) -> Response {
        let code = match self {
            ProposerApiError::NoBidPrepared |
            ProposerApiError::NoBids |
            ProposerApiError::BidValueZero |
            ProposerApiError::GetHeaderRequestTooLate { .. } |
            ProposerApiError::GetHeaderDisabledForProposer |
            ProposerApiError::NotServingHeaders => StatusCode::NO_CONTENT,

            ProposerApiError::HyperError(_) |
            ProposerApiError::AxumError(_) |
            ProposerApiError::ToStrError(_) |
            ProposerApiError::BidPublicKeyMismatch { .. } |
            ProposerApiError::MissingOpenBid |
            ProposerApiError::MissingProposer(_) |
            ProposerApiError::UnexpectedProposerIndex { .. } |
            ProposerApiError::NoValidatorsCouldBeRegistered |
            ProposerApiError::MissingPreferences(_) |
            ProposerApiError::MissingPayload(_) |
            ProposerApiError::InvalidGasLimit |
            ProposerApiError::InvalidFork |
            ProposerApiError::SerdeDecodeError(_) |
            ProposerApiError::UnknownBlock |
            ProposerApiError::UnknownBid |
            ProposerApiError::UnknownFeeRecipient(..) |
            ProposerApiError::ValidatorNotRegistered(..) |
            ProposerApiError::UnknownValidator(..) |
            ProposerApiError::InvalidSignature |
            ProposerApiError::ProposerNotRegistered |
            ProposerApiError::TimestampTooEarly { .. } |
            ProposerApiError::TimestampTooFarInTheFuture { .. } |
            ProposerApiError::NoPreviousRegistrationTimestamp |
            ProposerApiError::RegistrationTimestampTooOld |
            ProposerApiError::RequestWrongSlot { .. } |
            ProposerApiError::SlotTooNew |
            ProposerApiError::InvalidPayloadSignature |
            ProposerApiError::GetPayloadRequestTooLate { .. } |
            ProposerApiError::InvalidExecutionPayloadSignature |
            ProposerApiError::InvalidBeaconChainVersion |
            ProposerApiError::PayloadTypeMismatch |
            ProposerApiError::BlindedBlockAndPayloadHeaderMismatch |
            ProposerApiError::UnsupportedBeaconChainVersion |
            ProposerApiError::BeaconClientError(_) |
            ProposerApiError::DatabaseError(_) |
            ProposerApiError::AuctioneerError(_) |
            ProposerApiError::EmptyRequest |
            ProposerApiError::BlindedBlobsBundleLengthMismatch |
            ProposerApiError::InternalSlotMismatchesWithSlotDuty { .. } |
            ProposerApiError::InvalidBlindedBlockSlot { .. } |
            ProposerApiError::InvalidBlindedBlockParentHash { .. } |
            ProposerApiError::ParentHashUnknownForSlot { .. } |
            ProposerApiError::BlobKzgCommitmentsMismatch |
            ProposerApiError::SszError(_) |
            ProposerApiError::SigError(_) => StatusCode::BAD_REQUEST,

            ProposerApiError::InvalidApiKey => StatusCode::UNAUTHORIZED,

            ProposerApiError::InternalServerError | ProposerApiError::NoExecutionPayloadFound => {
                StatusCode::INTERNAL_SERVER_ERROR
            }

            ProposerApiError::ServiceUnavailableError => StatusCode::SERVICE_UNAVAILABLE,
        };

        (code, self.to_string()).into_response()
    }
}
