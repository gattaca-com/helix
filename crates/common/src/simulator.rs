use thiserror::Error;

#[derive(Debug, Clone, Error, serde::Serialize, serde::Deserialize)]
pub enum BlockSimError {
    #[error("block validation failed. Reason: {0}")]
    BlockValidationFailed(String),

    #[error("validation request timeout")]
    Timeout,

    #[error("rpc error. {0}")]
    RpcError(String),

    #[error("tokio::mpsc send error")]
    SendError,
}
