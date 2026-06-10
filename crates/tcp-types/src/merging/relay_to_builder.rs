use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{beacon::BlsPublicKey, engine::ExecutionPayloadV3};
use ssz_derive::{Decode, Encode};

use super::order::{MergeableOrder, OrderMeta};

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

/// Stores an appendable base block. The relay may send several distinct base
/// blocks per slot; `ActivateBaseBlockV1` selects the merge base.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct BaseBlockV1 {
    pub slot: u64,
    /// Originating (winning) builder.
    pub builder_pubkey: BlsPublicKey,
    /// Proposer payment carried by the last tx; validated exactly.
    pub original_value: U256,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: B256,
    pub execution_payload: ExecutionPayloadV3,
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct ActivateBaseBlockV1 {
    pub slot: u64,
    pub block_hash: B256,
}

/// Incremental, batched order stream. Add-only within a slot; dedup keys on
/// `OrderMeta::order_hash`.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct OrdersV1 {
    pub slot: u64,
    pub orders: Vec<OrderWithMeta>,
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct OrderWithMeta {
    pub meta: OrderMeta,
    pub order: MergeableOrder,
}

/// Re-binds existing orders (matched on `order_hash`) to a new builder /
/// origin / source block without re-sending tx bytes. Sent when a duplicate
/// order arrives in a higher-value block.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct UpdateOrderOriginsV1 {
    pub slot: u64,
    pub updates: Vec<OrderMeta>,
}

/// Early stop for the slot (payload delivered / auction over).
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct SlotEndV1 {
    pub slot: u64,
}
