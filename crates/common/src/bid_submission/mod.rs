pub mod submission;
pub mod v2;
pub mod v3;

use alloy_primitives::{Address, B256, U256};
use helix_types::{
    BidTrace, Bloom, BlsPublicKeyBytes, BlsSignatureBytes, ExtraData, ForkName, Slot,
};

#[auto_impl::auto_impl(Arc)]
pub trait BidSubmission {
    fn bid_trace(&self) -> &BidTrace;

    fn signature(&self) -> &BlsSignatureBytes;

    fn slot(&self) -> Slot;

    fn parent_hash(&self) -> &B256;

    fn block_hash(&self) -> &B256;

    fn builder_public_key(&self) -> &BlsPublicKeyBytes;

    fn proposer_public_key(&self) -> &BlsPublicKeyBytes;

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

    fn fork_name(&self) -> ForkName;
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

#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
}

impl OptimisticVersion {
    pub fn is_optimistic(&self) -> bool {
        matches!(self, Self::V1 | Self::V2 | Self::V3)
    }
}
