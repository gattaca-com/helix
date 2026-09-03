use alloy_primitives::{Address, B256};

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("block hash mismatch: got {got}, expected {expected}")]
    BlockHashMismatch { got: B256, expected: B256 },
    #[error("block parent hash mismatch: got {got}, expected {expected}")]
    ParentHashMismatch { got: B256, expected: B256 },
    #[error("block gas limit mismatch: got {got}, expected {expected}")]
    GasLimitMismatch { got: u64, expected: u64 },
    #[error("block gas used mismatch: got {got}, expected {expected}")]
    GasUsedMismatch { got: u64, expected: u64 },
    #[error("could not decode transaction: {0}")]
    DecodeTransaction(String),
    #[error("base fee per gas exceeds u64")]
    BaseFeeTooLarge,
    // Text matched by `BlockSimError::is_temporary`; changing it demotes builders.
    #[error("parent block not found")]
    MissingParentBlock,
    // Text matched by `BlockSimError::is_too_old`.
    #[error("block is too old, outside validation window")]
    BlockTooOld,
    #[error("store error: {0}")]
    Store(String),
    #[error("invalid block: {0}")]
    PreExecution(String),
    #[error("execution failed: {0}")]
    Execution(String),
    #[error("invalid block: {0}")]
    PostExecution(String),
    #[error("block state root mismatch: got {got}, expected {expected}")]
    StateRootMismatch { got: B256, expected: B256 },
    #[error("parent state is not available")]
    MissingParentState,
    #[error("could not verify proposer payment")]
    ProposerPayment,
    #[error("block accesses blacklisted address: {0}")]
    Blacklist(Address),
    #[error("invalid blobs bundle")]
    InvalidBlobsBundle,
    #[error("submission carries an empty block access list")]
    EmptyBlockAccessList,
}
