use axum::response::{IntoResponse, Response};
use ethereum_consensus::{
    primitives::{BlsPublicKey, Bytes32, Hash32},
    ssz::{prelude::*, self},
};
use hyper::StatusCode;
use helix_datastore::error::AuctioneerError;
use helix_common::simulator::BlockSimError;

#[derive(Debug, thiserror::Error)]
pub enum BuilderApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("ssz deserialize error: {0}")]
    SszDeserializeError(#[from] ssz::prelude::DeserializeError),

    #[error("payload too large. max size: {max_size}, size: {size}")]
    PayloadTooLarge { max_size: usize, size: usize },

    #[error("submission for past slot. current slot: {current_slot}, submission slot: {submission_slot}")]
    SubmissionForPastSlot { current_slot: u64, submission_slot: u64 },

    #[error("builder blacklisted. pubkey: {pubkey:?}")]
    BuilderBlacklisted { pubkey: BlsPublicKey },

    #[error("incorrect timestamp. got: {got}, expected: {expected}")]
    IncorrectTimestamp { got: u64, expected: u64 },

    #[error("could not find proposer duty for slot")]
    ProposerDutyNotFound,

    #[error("payload attributes not yet known")]
    PayloadAttributesNotYetKnown,

    #[error("payload slot mismatches with current payload attributes slot. got: {got}, expected: {expected}")]
    PayloadSlotMismatchWithPayloadAttributes {got: u64, expected: u64 },

    #[error("block hash mismatch. message: {message:?}, payload: {payload:?}")]
    BlockHashMismatch { message: Hash32, payload: Hash32 },

    #[error("parent hash mismatch. message: {message:?}, payload: {payload:?}")]
    ParentHashMismatch { message: Hash32, payload: Hash32 },

    #[error("fee recipient mismatch. got: {got:?}, expected: {expected:?}")]
    FeeRecipientMismatch { got: ByteVector<20>, expected: ByteVector<20> },

    #[error("slot mismatch. got: {got}, expected: {expected}")]
    SlotMismatch { got: u64, expected: u64 },

    #[error("zero value block")]
    ZeroValueBlock,

    #[error("missing withdrawls")]
    MissingWithdrawls,

    #[error("withdrawls root mismatch. got: {got:?}, expected: {expected:?}")]
    WithdrawalsRootMismatch { got: [u8; 32], expected: [u8; 32] },

    #[error("signature verification failed")]
    SignatureVerificationFailed,

    #[error("payload already delivered")]
    PayloadAlreadyDelivered,

    #[error("already processing newer payload")]
    AlreadyProcessingNewerPayload,

    #[error("accepted bid below floor, skipped validation")]
    BidBelowFloor,

    #[error("block validation error: {0:?}")]
    BlockValidationError(#[from] BlockSimError),

    #[error("internal error")]
    InternalError,

    #[error("datastore error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("incorrect prev_randao - got: {got:?}, expected: {expected:?}")]
    PrevRandaoMismatch { got: Bytes32, expected: Bytes32 },

    #[error("block already received: {block_hash:?}")]
    DuplicateBlockHash { block_hash: Hash32 },
}

impl IntoResponse for BuilderApiError {
    fn into_response(self) -> Response {
        match self {
            BuilderApiError::SerdeDecodeError(err) => {
                (StatusCode::BAD_REQUEST, format!("Serde decode error: {err}")).into_response()
            },
            BuilderApiError::IOError(err) => {
                (StatusCode::BAD_REQUEST, format!("IO error: {err}")).into_response()
            },
            BuilderApiError::SszDeserializeError(err) => {
                (StatusCode::BAD_REQUEST, format!("SSZ deserialize error: {err}")).into_response()
            },
            BuilderApiError::HyperError(err) => {
                (StatusCode::BAD_REQUEST, format!("Hyper error: {err}")).into_response()
            },
            BuilderApiError::PayloadTooLarge{ max_size, size } => {
                (StatusCode::BAD_REQUEST, format!("Payload too large. max size: {max_size}, size: {size}")).into_response()
            },
            BuilderApiError::SubmissionForPastSlot { current_slot, submission_slot } => {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Submission for past slot. current slot: {current_slot}, submission slot: {submission_slot}"),
                ).into_response()
            },
            BuilderApiError::BuilderBlacklisted{ pubkey } => {
                (StatusCode::BAD_REQUEST, format!("Builder blacklisted. pubkey: {pubkey:?}")).into_response()
            },
            BuilderApiError::IncorrectTimestamp { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("Incorrect timestamp. got: {got}, expected: {expected}")).into_response()
            },
            BuilderApiError::ProposerDutyNotFound => {
                (StatusCode::BAD_REQUEST, "Could not find proposer duty for slot").into_response()
            },
            BuilderApiError::PayloadSlotMismatchWithPayloadAttributes { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("payload slot mismatches with current payload attributes slot. got: {got}, expected: {expected}")).into_response()
            },
            BuilderApiError::BlockHashMismatch { message, payload } => {
                (StatusCode::BAD_REQUEST, format!("Block hash mismatch. message: {message:?}, payload: {payload:?}")).into_response()
            },
            BuilderApiError::ParentHashMismatch { message, payload } => {
                (StatusCode::BAD_REQUEST, format!("Parent hash mismatch. message: {message:?}, payload: {payload:?}")).into_response()
            },
            BuilderApiError::ZeroValueBlock => {
                (StatusCode::BAD_REQUEST, "Zero value block").into_response()
            },
            BuilderApiError::SignatureVerificationFailed => {
                (StatusCode::BAD_REQUEST, "Signature verification failed").into_response()
            },
            BuilderApiError::PayloadAlreadyDelivered => {
                (StatusCode::BAD_REQUEST, "Payload already delivered").into_response()
            },
            BuilderApiError::BidBelowFloor => {
                (StatusCode::ACCEPTED, "Bid below floor, skipped validation").into_response()
            },
            BuilderApiError::InternalError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            },
            BuilderApiError::BlockValidationError(err) => {
                match err {
                    BlockSimError::Timeout => {
                        (StatusCode::GATEWAY_TIMEOUT, "Block validation timeout").into_response()
                    },
                    _ => {
                        (StatusCode::BAD_REQUEST, format!("Block validation error: {err}")).into_response()
                    }
                }
            },
            BuilderApiError::AlreadyProcessingNewerPayload => {
                (StatusCode::BAD_REQUEST, "Already processing newer payload").into_response()
            },
            BuilderApiError::AuctioneerError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Auctioneer error: {err}")).into_response()
            },
            BuilderApiError::FeeRecipientMismatch { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("Fee recipient mismatch. got: {got:?}, expected: {expected:?}")).into_response()
            },
            BuilderApiError::SlotMismatch { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("Slot mismatch. got: {got}, expected: {expected}")).into_response()
            },
            BuilderApiError::MissingWithdrawls => {
                (StatusCode::BAD_REQUEST, "missing withdrawals").into_response()
            },
            BuilderApiError::WithdrawalsRootMismatch { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("Withdrawals root mismatch. got: {got:?}, expected: {expected:?}")).into_response()
            },
            BuilderApiError::PayloadAttributesNotYetKnown => {
                (StatusCode::BAD_REQUEST, "payload attributes not yet known").into_response()
            },
            BuilderApiError::PrevRandaoMismatch { got, expected } => {
                (StatusCode::BAD_REQUEST, format!("Prev randao mismatch. got: {got:?}, expected: {expected:?}")).into_response()
            },
            BuilderApiError::DuplicateBlockHash { block_hash } => {
                (StatusCode::BAD_REQUEST, format!("block already received: {block_hash:?}")).into_response()
            },
        }
    }
}
