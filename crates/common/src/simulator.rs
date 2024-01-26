use thiserror::Error;

const UNKNOWN_ANCESTOR: &str = "unknown ancestor";

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

impl BlockSimError {
    pub fn is_severe(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                match reason.to_lowercase().as_str() {
                    UNKNOWN_ANCESTOR => false,
                    _ => true,
                }
            },
            _ => true,
        }
    }
}