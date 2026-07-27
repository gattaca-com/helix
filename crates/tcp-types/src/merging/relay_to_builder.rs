use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{beacon::BlsPublicKey, engine::ExecutionPayloadV3};
use ssz_derive::{Decode, Encode};

use super::order::MergeOrderRef;

/// Slot boundary and flush: the builder drops all merging state for slots
/// earlier than `slot`.
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct SlotStartV1 {
    pub slot: u64,
    /// Relay's view of the parent; cross-checked against the builder's
    /// synced head.
    pub parent_hash: B256,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: B256,
}

/// A forwarded builder submission carrying merging data. The builder derives
/// the slot's order pool from `merge_orders` (attribution goes to the
/// highest-value block on duplicates) and, when `allow_appending`, stores the
/// payload as a merge-base candidate so a later `ActivateBaseBlockV1` is the
/// only message on the top-bid critical path.
///
/// `execution_payload.transactions` entries are dehydrated relative to this
/// connection: an entry of `order::TX_HASH_REF_LEN` bytes
/// (`order::is_tx_hash_ref`) is `keccak256(rlp)` of a tx already sent once on
/// this same connection (by any block, from any originating builder — the
/// cache is shared, not partitioned per source), to be resolved from that
/// side's own cache; anything else is a raw tx, which the receiver caches by
/// its hash for later references. The cache is connection-scoped: a fresh
/// connection starts empty, so the relay must resend full bytes for anything
/// replayed after a reconnect rather than reusing hashes from before the
/// drop.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct MergeableBlockV1 {
    pub slot: u64,
    /// Originating builder.
    pub builder_pubkey: BlsPublicKey,
    /// Bid value: the proposer payment carried by the last tx; validated
    /// exactly when the block is activated as a merge base.
    pub block_value: U256,
    /// Coinbase credited for orders merged from this block.
    pub builder_address: Address,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: B256,
    /// Eligible as a merge base.
    pub allow_appending: bool,
    /// Index-based order references into `execution_payload.transactions`.
    pub merge_orders: Vec<MergeOrderRef>,
    pub execution_payload: ExecutionPayloadV3,
}

/// Selects the merge base among forwarded appendable blocks: the relay's
/// current top bid. Merged blocks are built on and returned for this block
/// exclusively.
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct ActivateBaseBlockV1 {
    pub slot: u64,
    pub block_hash: B256,
}

/// Early stop for the slot (payload delivered / auction over).
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct SlotEndV1 {
    pub slot: u64,
}
