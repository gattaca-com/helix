pub mod bid_trace;
pub mod cancellation;
pub mod submission;
pub mod v2;

pub use bid_trace::*;
pub use submission::*;

use ethereum_consensus::{
    altair::Bytes32,
    capella::Withdrawal,
    deneb::mainnet::{BYTES_PER_LOGS_BLOOM, MAX_EXTRA_DATA_BYTES},
    primitives::{BlsPublicKey, BlsSignature, ExecutionAddress, Hash32, Slot, U256},
    ssz::prelude::*,
    Fork,
};

use crate::proofs::InclusionProofs;

#[auto_impl::auto_impl(Arc)]
pub trait BidSubmission {
    fn proofs(&self) -> Option<&InclusionProofs>;

    fn bid_trace(&self) -> &BidTrace;

    fn signature(&self) -> &BlsSignature;

    fn slot(&self) -> Slot;

    fn parent_hash(&self) -> &Hash32;

    fn block_hash(&self) -> &Hash32;

    fn builder_public_key(&self) -> &BlsPublicKey;

    fn proposer_public_key(&self) -> &BlsPublicKey;

    fn proposer_fee_recipient(&self) -> &ExecutionAddress;

    fn gas_limit(&self) -> u64;

    fn gas_used(&self) -> u64;

    fn value(&self) -> U256;

    fn fee_recipient(&self) -> &ExecutionAddress;

    fn state_root(&self) -> &Bytes32;

    fn receipts_root(&self) -> &Bytes32;

    fn logs_bloom(&self) -> &ByteVector<BYTES_PER_LOGS_BLOOM>;

    fn prev_randao(&self) -> &Bytes32;

    fn block_number(&self) -> u64;

    fn timestamp(&self) -> u64;

    fn extra_data(&self) -> &ByteList<MAX_EXTRA_DATA_BYTES>;

    fn base_fee_per_gas(&self) -> U256;

    fn withdrawals(&self) -> Option<&[Withdrawal]>;

    fn withdrawals_root(&self) -> Option<Node>;

    fn transactions_root(&self) -> Option<Node>;

    fn consensus_version(&self) -> Fork;

    /// True if full submission payload, false if not (e.g. Optimistic V2)
    fn is_full_payload(&self) -> bool;
}
