use thiserror::Error;

const UNKNOWN_ANCESTOR: &str = "unknown ancestor";
const PARENT_NOT_FOUND: &str = "parent block not found";
const MISSING_TRIE_NODE: &str = "missing trie node";
const BLOCK_ALREADY_KNOWN: &str = "block already known";
const BLOCK_TOO_OLD: &str = "block is too old, outside validation window";
const BLOCK_REQ_REORG: &str = "block requires a reorg";

#[derive(Debug, Error)]
pub enum BlockSimError {
    #[error("block validation failed. Reason: {0}")]
    BlockValidationFailed(String),

    #[error("validation request timeout")]
    Timeout,

    #[error("rpc error. {0}")]
    RpcError(#[from] reqwest::Error),

    #[error("tokio::mpsc send error")]
    SendError,

    #[error("no simulator available")]
    NoSimulatorAvailable,

    #[error("simulation dropped")]
    SimulationDropped,
}

impl BlockSimError {
    pub fn is_severe(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => match reason.to_lowercase().as_str() {
                UNKNOWN_ANCESTOR => false,
                PARENT_NOT_FOUND => false,
                BLOCK_ALREADY_KNOWN => false,
                BLOCK_REQ_REORG => false,
                r if r.starts_with(MISSING_TRIE_NODE) => false,
                _ => true,
            },
            _ => true,
        }
    }

    pub fn is_temporary(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => match reason.to_lowercase().as_str() {
                UNKNOWN_ANCESTOR => true,
                PARENT_NOT_FOUND => true,
                BLOCK_REQ_REORG => true,
                r if r.starts_with(MISSING_TRIE_NODE) => true,
                _ => false,
            },
            BlockSimError::Timeout => true,
            BlockSimError::RpcError(_) => true,
            BlockSimError::NoSimulatorAvailable => true,
            _ => false,
        }
    }

    pub fn is_already_known(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                matches!(reason.to_lowercase().as_str(), BLOCK_ALREADY_KNOWN)
            }
            _ => false,
        }
    }

    pub fn is_too_old(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                matches!(reason.to_lowercase().as_str(), BLOCK_TOO_OLD)
            }
            _ => false,
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

        let error = BlockSimError::BlockValidationFailed("invalid merkle root (remote: 72dccf8dfd7d2ccd518a03abea41b5e5d0a542f814b1e2c56f288e6056a287dc local: 43069fde058f3df14e7e6191f7de244fc5336940c89eff9349c09466c75eaf4b) dberr: %!w(<nil>)".to_string());
        assert!(error.is_severe());

        let error = BlockSimError::Timeout;
        assert!(error.is_severe());

        // let error = BlockSimError::RpcError(reqwest::Error::new());
        // assert!(error.is_severe());

        let error = BlockSimError::SendError;
        assert!(error.is_severe());
    }
}
