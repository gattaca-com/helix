use thiserror::Error;

const UNKNOWN_ANCESTOR: &str = "unknown ancestor";
const MISSING_TRIE_NODE: &str = "missing trie node";

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
                    r if r.starts_with(MISSING_TRIE_NODE) => false,
                    _ => true,
                }
            }
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_is_severe() {
        let error = BlockSimError::BlockValidationFailed("unknown ancestor".to_string());
        assert!(!error.is_severe());
    
        let error = BlockSimError::BlockValidationFailed("missing trie node".to_string());
        assert!(!error.is_severe());
    
        let error = BlockSimError::BlockValidationFailed("missing trie node: 0x123".to_string());
        assert!(!error.is_severe());
    
        let error = BlockSimError::BlockValidationFailed("other reason".to_string());
        assert!(error.is_severe());
    
        let error = BlockSimError::Timeout;
        assert!(error.is_severe());
    
        let error = BlockSimError::RpcError("rpc error".to_string());
        assert!(error.is_severe());
    
        let error = BlockSimError::SendError;
        assert!(error.is_severe());
    }
}
