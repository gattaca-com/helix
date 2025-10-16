use alloy_primitives::B256;
use thiserror::Error;

const UNKNOWN_ANCESTOR: &str = "unknown ancestor";
const PARENT_NOT_FOUND: &str = "parent block not found";
const MISSING_TRIE_NODE: &str = "missing trie node";
const BLOCK_ALREADY_KNOWN: &str = "block already known";
const BLOCK_TOO_OLD: &str = "block is too old, outside validation window";
const BLOCK_REQ_REORG: &str = "block requires a reorg";
const PARENT_BLOCK_NOT_FOUND: &str = "could not find parent block: parent block not found";

#[derive(Debug, Clone, Error)]
pub enum BlockSimError {
    #[error("block validation failed. Reason: {0}")]
    BlockValidationFailed(String),

    #[error("invalid tx root: got {got}, expected: {expected}")]
    InvalidTxRoot { got: B256, expected: B256 },

    #[error("validation request timeout")]
    Timeout,

    #[error("rpc error")]
    RpcError,

    #[error("tokio::mpsc send error")]
    SendError,

    #[error("no simulator available")]
    NoSimulatorAvailable,

    #[error("simulation dropped")]
    SimulationDropped,
}

impl BlockSimError {
    pub fn is_temporary(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => match reason.to_lowercase().as_str() {
                UNKNOWN_ANCESTOR => true,
                PARENT_NOT_FOUND => true,
                PARENT_BLOCK_NOT_FOUND => true,
                BLOCK_REQ_REORG => true,
                r if r.starts_with(MISSING_TRIE_NODE) => true,
                _ => false,
            },
            BlockSimError::Timeout => true,
            BlockSimError::RpcError => true,
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

    pub fn is_demotable(&self) -> bool {
        !self.is_already_known() && !self.is_temporary() && !self.is_too_old()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temporary() {
        let s = String::from("could not find parent block: parent block not found");
        let err = BlockSimError::BlockValidationFailed(s);

        assert!(err.is_temporary())
    }

    #[test]
    fn test_demotable() {
        assert!(
            BlockSimError::InvalidTxRoot { got: Default::default(), expected: Default::default() }
                .is_demotable()
        )
    }
}
