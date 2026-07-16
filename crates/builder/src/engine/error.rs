use alloy_primitives::{Address, B256, U256};
use helix_tcp_types::merging::builder_to_relay::{RejectCode, RejectSubject};

/// Merge pipeline failures. Most map onto a `RejectV1`; the rest are internal
/// conditions that silently skip an emission.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("builder is not synced to the base block's parent")]
    NotSynced,
    #[error("base block parent does not match the builder's head")]
    HeadMismatch,
    #[error("last tx is not a proposer payment of the declared value")]
    InvalidPayment,
    #[error("no collateral safe for coinbase {0}")]
    UnknownCollateral(Address),
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    #[error("stale slot")]
    StaleSlot,
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("invalid base block: {0}")]
    InvalidBaseBlock(String),

    // Internal / non-reject conditions.
    #[error("safe {address} balance {current} below required {required}")]
    NoBalanceInBuilderSafe { address: Address, current: U256, required: U256 },
    #[error("revenue distribution transaction reverted")]
    RevenueAllocationReverted,
    #[error("builder balance delta mismatch: revenues {revenues}, delta {delta}")]
    BalanceDeltaMismatch { revenues: U256, delta: U256 },
    #[error("internal: {0}")]
    Internal(String),
}

impl MergeError {
    /// The `RejectV1` mapping for this error, or `None` for internal
    /// conditions that must not produce protocol traffic.
    pub fn reject(&self, block_hash: Option<B256>) -> Option<(RejectCode, RejectSubject)> {
        let subject = block_hash.map(RejectSubject::BlockHash).unwrap_or(RejectSubject::None(0));
        let code = match self {
            MergeError::NotSynced => RejectCode::NotSynced,
            MergeError::HeadMismatch => RejectCode::HeadMismatch,
            MergeError::InvalidPayment => RejectCode::InvalidPayment,
            MergeError::UnknownCollateral(_) => RejectCode::UnknownCollateral,
            MergeError::InvalidOrder(_) => RejectCode::InvalidOrder,
            MergeError::StaleSlot => RejectCode::StaleSlot,
            MergeError::LimitExceeded(_) => RejectCode::LimitExceeded,
            MergeError::InvalidBaseBlock(_) => RejectCode::InvalidBaseBlock,
            MergeError::NoBalanceInBuilderSafe { .. } |
            MergeError::RevenueAllocationReverted |
            MergeError::BalanceDeltaMismatch { .. } |
            MergeError::Internal(_) => return None,
        };
        Some((code, subject))
    }
}

/// Why a single order was skipped during (pre)simulation. Orders are best
/// effort: none of these fail the merge, they just exclude the order.
#[derive(Debug, thiserror::Error)]
pub enum SimulationError {
    #[error("zero builder payment")]
    ZeroBuilderPayment,
    #[error("gas used exceeds allotted block limit")]
    OutOfBlockGas,
    #[error("blobs used exceed allotted block limit")]
    OutOfBlockBlobs,
    #[error("duplicate transaction in bundle")]
    DuplicateTransaction,
    #[error("transaction {_0} reverted and is not allowed to revert")]
    RevertNotAllowed(usize),
    #[error("transaction {_0} is invalid and can't be dropped")]
    DropNotAllowed(usize),
    #[error("execution error: {_0}")]
    Execution(String),
}
