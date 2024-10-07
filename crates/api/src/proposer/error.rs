use std::{error::Error, fmt};

use axum::{
    http::{self, StatusCode},
    response::{IntoResponse, Response},
};
use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, Hash32, Slot},
    ssz::prelude::MerkleizationError,
};
use helix_beacon_client::error::BeaconClientError;
use helix_database::error::DatabaseError;
use helix_datastore::error::AuctioneerError;
use serde::{Deserialize, Serialize};
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
    BidPublicKeyMismatch { bid: BlsPublicKey, relay: BlsPublicKey },

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
    UnexpectedProposerIndex { expected: usize, actual: usize },

    #[error("no validators could be registered")]
    NoValidatorsCouldBeRegistered,

    #[error("no preferences found for validator with public key {0:?}")]
    MissingPreferences(BlsPublicKey),

    #[error("no payload returned for opened bid with block hash {0:?}")]
    MissingPayload(Hash32),

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
    UnknownFeeRecipient(BlsPublicKey, ExecutionAddress),

    #[error("validator with public key {0:?} is not currently registered")]
    ValidatorNotRegistered(BlsPublicKey),

    #[error("validator with public key {0:?} is unknown")]
    UnknownValidator(BlsPublicKey),

    #[error("invalid signature: {0}")]
    InvalidSignature(#[from] ethereum_consensus::Error),

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

    #[error("request for past slot. request slot: {request_slot}, head slot: {head_slot}")]
    RequestForPastSlot { request_slot: u64, head_slot: u64 },

    #[error("preconfer election must be for current epoch + 1. request epoch: {request_epoch}, current epoch: {head_epoch}")]
    ElectPreconferRequestForInvalidEpoch { request_epoch: u64, head_epoch: u64 },

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

    #[error("invalid execution payload header: {0}")]
    InvalidExecutionPayloadHeader(ethereum_consensus::Error),

    #[error("merkleization error: {0}")]
    MerkleizationError(#[from] MerkleizationError),

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

    #[error("number of blinded blobs does not match blobs bundle length")]
    BlindedBlobsBundleLengthMismatch,

    #[error("internal slot: {internal_slot} does not match slot duty slot: {slot_duty_slot}")]
    InternalSlotMismatchesWithSlotDuty { internal_slot: u64, slot_duty_slot: u64 },

    #[error("internal slot: {internal_slot} does not match blinded block slot: {blinded_block_slot}")]
    InvalidBlindedBlockSlot { internal_slot: u64, blinded_block_slot: u64 },

    #[error("expected parent hash: {expected_parent_hash:?} does not match blinded block parent hash: {blinded_block_parent_hash:?}")]
    InvalidBlindedBlockParentHash { expected_parent_hash: Hash32, blinded_block_parent_hash: Hash32 },

    #[error("parent hash unknown for slot: {slot}")]
    ParentHashUnknownForSlot { slot: u64 },

    #[error("lock poisoned")]
    LockPoisoned,

    #[error("no preconfer found for slot: {slot}")]
    NoPreconferFoundForSlot { slot: u64 },

    #[error("can only set constraints for current epoch. request slot: {request_slot}, curr slot: {curr_slot}")]
    CanOnlySetConstraintsForCurrentEpoch { request_slot: u64, curr_slot: u64 },

    #[error("set constraints sent too late. ns into slot: {ns_into_slot}, cutoff: {cutoff}")]
    SetConstraintsTooLate { ns_into_slot: u64, cutoff: u64 },

    #[error("not preconfer. request public key: {request_public_key:?}, preconfer public key: {preconfer_public_key:?}")]
    NotPreconfer { request_public_key: BlsPublicKey, preconfer_public_key: BlsPublicKey },

    #[error("validator is not proposer for requested slot")]
    ValidatorIsNotProposerForRequestedSlot,

    #[error("proposer duty not found. slot: {slot}")]
    ProposerDutyNotFound { slot: u64 },

    #[error("constraints are already set for slot {slot}")]
    ConstraintsAlreadySet { slot: u64 },
}

impl IntoResponse for ProposerApiError {
    fn into_response(self) -> Response {
        match self {
            ProposerApiError::HyperError(err) => (StatusCode::BAD_REQUEST, format!("Hyper error: {err}")).into_response(),
            ProposerApiError::AxumError(err) => (StatusCode::BAD_REQUEST, format!("Axum error: {err}")).into_response(),
            ProposerApiError::ToStrError(err) => (StatusCode::BAD_REQUEST, format!("ToStr error: {err}")).into_response(),
            ProposerApiError::BidPublicKeyMismatch { bid, relay } => {
                (StatusCode::BAD_REQUEST, format!("Bid public key {bid:?} does not match relay public key {relay:?}")).into_response()
            }
            ProposerApiError::NoBidPrepared => (StatusCode::NO_CONTENT, "No bid prepared for request").into_response(),
            ProposerApiError::NoBids => (StatusCode::NO_CONTENT, "No valid bids returned for proposal").into_response(),
            ProposerApiError::BidValueZero => (StatusCode::NO_CONTENT, "Bid has value 0").into_response(),
            ProposerApiError::MissingOpenBid => (StatusCode::BAD_REQUEST, "Could not find relay with outstanding bid to accept").into_response(),
            ProposerApiError::MissingProposer(slot) => (StatusCode::BAD_REQUEST, format!("Could not find proposer for slot {slot}")).into_response(),
            ProposerApiError::UnexpectedProposerIndex { expected, actual } => {
                (StatusCode::BAD_REQUEST, format!("unexpected proposer index. expected: {expected}. actual: {actual}")).into_response()
            }
            ProposerApiError::NoValidatorsCouldBeRegistered => (StatusCode::BAD_REQUEST, "No validators could be registered").into_response(),
            ProposerApiError::MissingPreferences(pubkey) => {
                (StatusCode::BAD_REQUEST, format!("No preferences found for validator with public key {pubkey:?}")).into_response()
            }
            ProposerApiError::MissingPayload(hash) => {
                (StatusCode::BAD_REQUEST, format!("No payload returned for opened bid with block hash {hash:?}")).into_response()
            }
            ProposerApiError::InvalidGasLimit => {
                (StatusCode::BAD_REQUEST, "Payload gas limit does not match the proposer's preference").into_response()
            }
            ProposerApiError::InvalidFork => (StatusCode::BAD_REQUEST, "Data for an unexpected fork was provided").into_response(),
            ProposerApiError::SerdeDecodeError(err) => (StatusCode::BAD_REQUEST, format!("Serde decode error: {err}")).into_response(),
            ProposerApiError::UnknownBlock => (StatusCode::BAD_REQUEST, "Block does not match the provided header").into_response(),
            ProposerApiError::UnknownBid => (StatusCode::BAD_REQUEST, "Payload request does not match any outstanding bid").into_response(),
            ProposerApiError::UnknownFeeRecipient(pubkey, addr) => {
                (StatusCode::BAD_REQUEST, format!("Validator: {pubkey:?} does not have {addr:?} fee recipient")).into_response()
            }
            ProposerApiError::ValidatorNotRegistered(pubkey) => {
                (StatusCode::BAD_REQUEST, format!("Validator with public key {pubkey:?} is not currently registered")).into_response()
            }
            ProposerApiError::UnknownValidator(pubkey) => {
                (StatusCode::BAD_REQUEST, format!("Validator with public key {pubkey:?} is unknown")).into_response()
            }
            ProposerApiError::InvalidSignature(err) => (StatusCode::BAD_REQUEST, format!("Invalid signature. {err:?}")).into_response(),
            ProposerApiError::ProposerNotRegistered => (StatusCode::BAD_REQUEST, "proposer not registered").into_response(),
            ProposerApiError::InvalidApiKey => (StatusCode::UNAUTHORIZED, "invalid api key").into_response(),
            ProposerApiError::TimestampTooEarly { timestamp, min_timestamp } => {
                (StatusCode::BAD_REQUEST, format!("Timestamp too early. {timestamp} < {min_timestamp}")).into_response()
            }
            ProposerApiError::TimestampTooFarInTheFuture { timestamp, max_timestamp } => {
                (StatusCode::BAD_REQUEST, format!("Timestamp too far in the future. {timestamp} > {max_timestamp}")).into_response()
            }
            ProposerApiError::NoPreviousRegistrationTimestamp => (StatusCode::BAD_REQUEST, "No previous registration timestamp").into_response(),
            ProposerApiError::RegistrationTimestampTooOld => (StatusCode::BAD_REQUEST, "Registration timestamp too old").into_response(),
            ProposerApiError::RequestForPastSlot { request_slot, head_slot } => {
                (StatusCode::BAD_REQUEST, format!("request for past slot. request slot: {request_slot}, head slot: {head_slot}")).into_response()
            }
            ProposerApiError::SlotTooNew => (StatusCode::BAD_REQUEST, "Slot is too new").into_response(),
            ProposerApiError::GetHeaderRequestTooLate { ms_into_slot, cutoff } => {
                (StatusCode::NO_CONTENT, format!("getHeader sent too late. ms into slot: {ms_into_slot}, cutoff: {cutoff}")).into_response()
            }
            ProposerApiError::InvalidPayloadSignature => (StatusCode::BAD_REQUEST, "Could not verify payload signature").into_response(),
            ProposerApiError::GetPayloadRequestTooLate { cutoff, request_time } => {
                (StatusCode::BAD_REQUEST, format!("sent too late. cutoff: {cutoff}, request time into slot: {request_time}")).into_response()
            }
            ProposerApiError::NoExecutionPayloadFound => (StatusCode::INTERNAL_SERVER_ERROR, "No execution payload for this request").into_response(),
            ProposerApiError::InvalidExecutionPayloadSignature => {
                (StatusCode::BAD_REQUEST, "Could not verify execution payload signature").into_response()
            }
            ProposerApiError::InvalidExecutionPayloadHeader(err) => {
                (StatusCode::BAD_REQUEST, format!("Invalid execution payload header: {err}")).into_response()
            }
            ProposerApiError::MerkleizationError(err) => (StatusCode::BAD_REQUEST, format!("Merkleization error: {err}")).into_response(),
            ProposerApiError::InvalidBeaconChainVersion => (StatusCode::BAD_REQUEST, "Invalid beacon chain version").into_response(),
            ProposerApiError::PayloadTypeMismatch => (StatusCode::BAD_REQUEST, "payload type mismatch").into_response(),
            ProposerApiError::BlindedBlockAndPayloadHeaderMismatch => {
                (StatusCode::BAD_REQUEST, "Blinded block header hash does not match payload header hash").into_response()
            }
            ProposerApiError::UnsupportedBeaconChainVersion => (StatusCode::BAD_REQUEST, "unsupported beacon chain version").into_response(),
            ProposerApiError::BeaconClientError(err) => (StatusCode::BAD_REQUEST, format!("beacon client error: {err}")).into_response(),
            ProposerApiError::DatabaseError(err) => (StatusCode::BAD_REQUEST, format!("database error: {err}")).into_response(),
            ProposerApiError::AuctioneerError(err) => (StatusCode::BAD_REQUEST, format!("auctioneer error: {err}")).into_response(),
            ProposerApiError::EmptyRequest => (StatusCode::BAD_REQUEST, "empty request").into_response(),
            ProposerApiError::InternalServerError => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response(),
            ProposerApiError::BlindedBlobsBundleLengthMismatch => {
                (StatusCode::BAD_REQUEST, "number of blinded blobs does not match blobs bundle length").into_response()
            }
            ProposerApiError::InternalSlotMismatchesWithSlotDuty { internal_slot, slot_duty_slot } => {
                (StatusCode::BAD_REQUEST, format!("internal slot: {internal_slot} does not match slot duty slot: {slot_duty_slot}")).into_response()
            }
            ProposerApiError::InvalidBlindedBlockSlot { internal_slot, blinded_block_slot } => {
                (StatusCode::BAD_REQUEST, format!("internal slot: {internal_slot} does not match blinded block slot: {blinded_block_slot}"))
                    .into_response()
            }
            ProposerApiError::InvalidBlindedBlockParentHash { expected_parent_hash, blinded_block_parent_hash } => (
                StatusCode::BAD_REQUEST,
                format!("expected parent hash: {expected_parent_hash} does not match blinded block parent hash: {blinded_block_parent_hash}"),
            )
                .into_response(),
            ProposerApiError::ParentHashUnknownForSlot { slot } => {
                (StatusCode::BAD_REQUEST, format!("parent hash unknown for slot: {slot}")).into_response()
            }
            ProposerApiError::LockPoisoned => (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned").into_response(),
            ProposerApiError::NoPreconferFoundForSlot { slot } => {
                (StatusCode::BAD_REQUEST, format!("no preconfer found for slot: {slot}")).into_response()
            }
            ProposerApiError::CanOnlySetConstraintsForCurrentEpoch { request_slot, curr_slot } => {
                (StatusCode::BAD_REQUEST, format!("can only set constraints for current epoch. request slot: {request_slot}, curr slot: {curr_slot}"))
                    .into_response()
            }
            ProposerApiError::SetConstraintsTooLate { ns_into_slot, cutoff } => {
                (StatusCode::BAD_REQUEST, format!("set constraints sent too late. ns into slot: {ns_into_slot}, cutoff: {cutoff}")).into_response()
            }
            ProposerApiError::NotPreconfer { request_public_key, preconfer_public_key } => (
                StatusCode::BAD_REQUEST,
                format!("not preconfer. request public key: {request_public_key:?}, preconfer public key: {preconfer_public_key:?}"),
            )
                .into_response(),
            ProposerApiError::ValidatorIsNotProposerForRequestedSlot => {
                (StatusCode::BAD_REQUEST, "validator is not proposer for requested slot").into_response()
            }
            ProposerApiError::ProposerDutyNotFound { slot } => {
                (StatusCode::BAD_REQUEST, format!("proposer duty not found. slot: {slot}")).into_response()
            }
            ProposerApiError::ElectPreconferRequestForInvalidEpoch { request_epoch, head_epoch } => (
                StatusCode::BAD_REQUEST,
                format!("preconfer election must be for current epoch + 1. request epoch: {request_epoch}, current epoch: {head_epoch}"),
            )
                .into_response(),
            ProposerApiError::ConstraintsAlreadySet { slot } => {
                (StatusCode::BAD_REQUEST, format!("constraints are already set for slot {slot}")).into_response()
            }
        }
    }
}

// NOTE: `IndexedError` must come before `ErrorMessage` so
// the `serde(untagged)` machinery does not greedily match it first.
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum ApiError {
    IndexedError {
        #[serde(with = "as_u16")]
        code: StatusCode,
        message: String,
        failures: Vec<IndexedError>,
    },
    ErrorMessage {
        #[serde(with = "as_u16")]
        code: StatusCode,
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct IndexedError {
    index: usize,
    message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ErrorMessage { message, .. } => {
                write!(f, "{message}")
            }
            Self::IndexedError { message, failures, .. } => {
                write!(f, "{message}: ")?;
                for failure in failures {
                    write!(f, "{failure:?}, ")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ApiError {}

impl<'a> TryFrom<(u16, &'a str)> for ApiError {
    type Error = http::status::InvalidStatusCode;

    fn try_from((code, message): (u16, &'a str)) -> Result<Self, Self::Error> {
        let code = StatusCode::from_u16(code)?;
        Ok(Self::ErrorMessage { code, message: message.to_string() })
    }
}

pub(crate) mod as_u16 {
    use axum::http;
    use http::StatusCode;
    use serde::{de::Deserializer, Deserialize, Serializer};

    pub fn serialize<S>(x: &StatusCode, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u16(x.as_u16())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<StatusCode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: u16 = Deserialize::deserialize(deserializer)?;
        StatusCode::from_u16(value).map_err(serde::de::Error::custom)
    }
}
