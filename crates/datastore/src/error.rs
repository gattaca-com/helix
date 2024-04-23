use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ethereum_consensus::{primitives::BlsPublicKey, ssz};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::redis::error::RedisCacheError;

#[derive(Debug, thiserror::Error)]
pub enum AuctioneerError {
    #[error("unexpected value type")]
    UnexpectedValueType,

    #[error("broadcast stream recv error")]
    BroadcastStreamRecvError(#[from] BroadcastStreamRecvError),

    #[error("redis error: {0}")]
    RedisError(#[from] RedisCacheError),

    #[error("from utf8 error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    #[error("parse int error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("past slot already delivered")]
    PastSlotAlreadyDelivered,

    #[error("another payload already delivered for slot")]
    AnotherPayloadAlreadyDeliveredForSlot,

    #[error("ssz deserialize error: {0}")]
    SszDeserializeError(#[from] ssz::prelude::DeserializeError),

    #[error("ssz serialize error: {0}")]
    SszSerializeError(#[from] ssz::prelude::SerializeError),

    #[error("no execution payload for this request")]
    ExecutionPayloadNotFound,

    #[error("builder not found for pub key {pub_key:?}")]
    BuilderNotFound { pub_key: BlsPublicKey },

    #[error("ethereum consensus error: {0}")]
    EthereumConsensusError(#[from] ethereum_consensus::Error),

    #[error("ethereum consensus crypto error: {0}")]
    EthereumConsensusCryptoError(#[from] ethereum_consensus::crypto::Error),
}

impl IntoResponse for AuctioneerError {
    fn into_response(self) -> Response {
        match self {
            AuctioneerError::UnexpectedValueType => {
                (StatusCode::BAD_REQUEST, "Unexpected value type".to_string()).into_response()
            }
            AuctioneerError::RedisError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Redis error: {err}")).into_response()
            }
            AuctioneerError::FromUtf8Error(err) => {
                (StatusCode::BAD_REQUEST, format!("UTF-8 error: {err}")).into_response()
            }
            AuctioneerError::ParseIntError(err) => {
                (StatusCode::BAD_REQUEST, format!("Parse Int error: {err}")).into_response()
            }
            AuctioneerError::PastSlotAlreadyDelivered => {
                (StatusCode::BAD_REQUEST, "Past slot already delivered".to_string()).into_response()
            }
            AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot => {
                (StatusCode::BAD_REQUEST, "Another payload already delivered for slot".to_string())
                    .into_response()
            }
            AuctioneerError::SszDeserializeError(err) => {
                (StatusCode::BAD_REQUEST, format!("SSZ deserialize error: {err}")).into_response()
            }
            AuctioneerError::SszSerializeError(err) => {
                (StatusCode::BAD_REQUEST, format!("SSZ serialize error: {err}")).into_response()
            }
            AuctioneerError::ExecutionPayloadNotFound => {
                (StatusCode::BAD_REQUEST, "No execution payload for this request".to_string())
                    .into_response()
            }
            AuctioneerError::BuilderNotFound { pub_key } => {
                (StatusCode::BAD_REQUEST, format!("Builder not found for public key: {pub_key:?}"))
                    .into_response()
            }
            AuctioneerError::EthereumConsensusError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Ethereum consensus error: {err:?}"))
                    .into_response()
            }
            AuctioneerError::BroadcastStreamRecvError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Broadcast stream recv error: {err}"))
                    .into_response()
            }
            AuctioneerError::EthereumConsensusCryptoError(err) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Ethereum consensus error: {err:?}"))
                    .into_response()
            }
        }
    }
}
