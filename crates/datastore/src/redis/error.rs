use redis::RedisError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RedisCacheError {
    #[error("redis error: {0}")]
    RedisError(#[from] RedisError),

    #[error("redis pool error: {0}")]
    RedisPoolError(#[from] deadpool_redis::PoolError),

    #[error("redis create pool error: {0}")]
    CreatePoolError(#[from] deadpool_redis::CreatePoolError),

    #[error("redis copy error. Could not copy from {from} to {to}")]
    RedisCopyError { from: String, to: String },

    #[error("unexpected value type")]
    UnexpectedValueType,

    #[error("serde_json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    #[error("from utf8 error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    #[error("parse int error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("internal error")]
    InternalError,
}
