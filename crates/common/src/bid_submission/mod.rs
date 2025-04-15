pub mod submission;
pub mod v2;
pub mod v3;

use alloy_primitives::{Address, B256, U256};
use helix_types::{BidTrace, Bloom, BlsPublicKey, BlsSignature, ExtraData, Slot};

#[auto_impl::auto_impl(Arc)]
pub trait BidSubmission {
    fn bid_trace(&self) -> &BidTrace;

    fn signature(&self) -> &BlsSignature;

    fn slot(&self) -> Slot;

    fn parent_hash(&self) -> &B256;

    fn block_hash(&self) -> &B256;

    fn builder_public_key(&self) -> &BlsPublicKey;

    fn proposer_public_key(&self) -> &BlsPublicKey;

    fn proposer_fee_recipient(&self) -> &Address;

    fn gas_limit(&self) -> u64;

    fn gas_used(&self) -> u64;

    fn value(&self) -> U256;

    fn fee_recipient(&self) -> Address;

    fn state_root(&self) -> &B256;

    fn receipts_root(&self) -> &B256;

    fn logs_bloom(&self) -> &Bloom;

    fn prev_randao(&self) -> &B256;

    fn block_number(&self) -> u64;

    fn timestamp(&self) -> u64;

    fn extra_data(&self) -> &ExtraData;

    fn base_fee_per_gas(&self) -> U256;

    fn withdrawals_root(&self) -> B256;

    fn transactions_root(&self) -> B256;

    /// True if full submission payload, false if not (e.g. Optimistic V2)
    fn is_full_payload(&self) -> bool;

    /// Validates that the bid trace and execution payload are consistent
    fn validate(&self) -> Result<(), BidValidationError>;
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BidValidationError {
    #[error("block hash mismatch: message: {message:?}, payload: {payload:?}")]
    BlockHashMismatch { message: B256, payload: B256 },

    #[error("parent hash mismatch. message: {message:?}, payload: {payload:?}")]
    ParentHashMismatch { message: B256, payload: B256 },

    #[error("gas limit mismatch. message: {message:?}, payload: {payload:?}")]
    GasLimitMismatch { message: u64, payload: u64 },

    #[error("gas used mismatch. message: {message:?}, payload: {payload:?}")]
    GasUsedMismatch { message: u64, payload: u64 },

    #[error("zero value block")]
    ZeroValueBlock,
}
