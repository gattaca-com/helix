use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use helix_types::{BlsPublicKey, CryptoError};

use crate::redis::error::RedisCacheError;

#[derive(Debug, thiserror::Error)]
pub enum AuctioneerError {
    #[error("unexpected value type")]
    UnexpectedValueType,

    #[error("crypto error: {0:?}")]
    CryptoError(CryptoError),

    #[error("redis error: {0}")]
    RedisError(#[from] RedisCacheError),

    #[error("from utf8 error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    #[error("parse int error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("from hex error: {0}")]
    FromHexError(#[from] alloy_primitives::hex::FromHexError),

    #[error("past slot already delivered")]
    PastSlotAlreadyDelivered,

    #[error("another payload already delivered for slot")]
    AnotherPayloadAlreadyDeliveredForSlot,

    #[error("ssz deserialize error: {0:?}")]
    SszDeserializeError(ssz::DecodeError),

    #[error("Slice conversion error: {0:?}")]
    SliceConversionError(#[from] core::array::TryFromSliceError),

    #[error("no execution payload for this request")]
    ExecutionPayloadNotFound,

    #[error("builder not found for pubkey {pub_key:?}")]
    BuilderNotFound { pub_key: BlsPublicKey },
}

impl IntoResponse for AuctioneerError {
    fn into_response(self) -> Response {
        let code = match self {
            AuctioneerError::UnexpectedValueType |
            AuctioneerError::CryptoError(_) |
            AuctioneerError::FromUtf8Error(_) |
            AuctioneerError::ParseIntError(_) |
            AuctioneerError::FromHexError(_) |
            AuctioneerError::PastSlotAlreadyDelivered |
            AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot |
            AuctioneerError::SszDeserializeError(_) |
            AuctioneerError::SliceConversionError(_) |
            AuctioneerError::ExecutionPayloadNotFound |
            AuctioneerError::BuilderNotFound { .. } => StatusCode::BAD_REQUEST,

            AuctioneerError::RedisError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (code, self.to_string()).into_response()
    }
}
