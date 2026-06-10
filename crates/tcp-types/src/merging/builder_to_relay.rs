use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{
    beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use ssz_derive::{Decode, Encode};

pub const MAX_APPENDED_BLOBS: usize = 128;
pub const MAX_BUILDER_INCLUSIONS: usize = 256;
pub const MAX_INCLUSION_TXS: usize = 1024;
pub const MAX_INCLUDED_ORDER_IDS: usize = 16_384;

/// Streamed merged result. Sent only when `proposer_value` strictly improves
/// on the best previously sent for `(slot, base_block_hash)`.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct MergedBlockV1 {
    pub slot: u64,
    /// Monotonic per connection.
    pub response_id: u32,
    /// Base block the merged block was built on.
    pub base_block_hash: B256,
    pub base_builder_pubkey: BlsPublicKey,
    pub execution_payload: ExecutionPayloadV3,
    pub execution_requests: ExecutionRequestsV4,
    /// Versioned hashes of appended blob txs, in append order. The relay
    /// re-attaches sidecars from its own store.
    pub appended_blobs: Vec<B256>,
    /// `original_value` + proposer share of merged revenue.
    pub proposer_value: U256,
    pub builder_inclusions: Vec<BuilderInclusion>,
    /// `OrderMeta::order_id()` of every merged order, traceable to the
    /// contributing builder and source block.
    pub included_order_ids: Vec<B256>,
    pub trace: MergeTraceV1,
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct BuilderInclusion {
    pub builder_pubkey: BlsPublicKey,
    pub origin_coinbase: Address,
    pub revenue: U256,
    pub txs: Vec<B256>,
}

/// Nanosecond timestamps on the builder's clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Encode, Decode)]
pub struct MergeTraceV1 {
    pub base_block_recv_ns: u64,
    pub sim_start_ns: u64,
    pub sim_end_ns: u64,
    pub finalize_ns: u64,
}

/// Non-fatal rejection; the connection stays up.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct RejectV1 {
    pub slot: u64,
    pub code: RejectCode,
    pub subject: RejectSubject,
    pub msg: Vec<u8>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[ssz(enum_behaviour = "tag")]
pub enum RejectCode {
    /// Base block parent/number/timestamp != builder's synced head.
    HeadMismatch = 0,
    /// Builder is behind; the relay may resend.
    NotSynced = 1,
    /// `ActivateBaseBlockV1` for a hash never stored.
    UnknownBaseBlock = 2,
    /// Last tx is not a proposer payment of `original_value`.
    InvalidPayment = 3,
    /// Base fee recipient not in `builder_collaterals`.
    UnknownCollateral = 4,
    /// Failed tx validation / bad indices / order_hash mismatch; order dropped.
    InvalidOrder = 5,
    StaleSlot = 6,
    LimitExceeded = 7,
    Busy = 8,
    /// Base block failed tx validation; block refused.
    InvalidBaseBlock = 9,
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
#[ssz(enum_behaviour = "union")]
pub enum RejectSubject {
    BlockHash(B256),
    OrderHash(B256),
    None(u8),
}

/// Best-effort notification before disconnect.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct FatalV1 {
    pub code: RejectCode,
    pub msg: Vec<u8>,
}
