use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{
    beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use ssz_derive::{Decode, Encode};
use strum::{AsRefStr, EnumString};

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
    /// Value paid to the base block builder for its share of the merge split.
    pub base_builder_revenue: U256,
    /// Value paid to the relay for its share of the merge split.
    pub relay_revenue: U256,
    pub builder_inclusions: Vec<BuilderInclusion>,
    /// `OrderMeta::order_id()` of every merged order, traceable to the
    /// contributing builder and source block.
    pub included_order_ids: Vec<B256>,
    pub trace: MergeTraceV1,
    pub unmerged_txs: Vec<UnmergedTx>,
}

/// `MergedBlockV1` for an Amsterdam base: the same fields at the same offsets,
/// plus what the Fulu-shaped payload cannot carry. Sent in place of V1 only
/// when the block needs them.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct MergedBlockV2 {
    pub slot: u64,
    pub response_id: u32,
    pub base_block_hash: B256,
    pub base_builder_pubkey: BlsPublicKey,
    pub execution_payload: ExecutionPayloadV3,
    /// EIP-7928 list as RLP, the exact bytes the header's hash commits to.
    pub block_access_list: Vec<u8>,
    /// Type-prefixed EIP-7685 entries, so EIP-8282's types 3 and 4 fit.
    pub execution_requests: Vec<Vec<u8>>,
    pub appended_blobs: Vec<B256>,
    pub proposer_value: U256,
    pub base_builder_revenue: U256,
    pub relay_revenue: U256,
    pub builder_inclusions: Vec<BuilderInclusion>,
    pub included_order_ids: Vec<B256>,
    pub trace: MergeTraceV1,
    pub unmerged_txs: Vec<UnmergedTx>,
}

impl From<MergedBlockV1> for MergedBlockV2 {
    fn from(m: MergedBlockV1) -> Self {
        let requests = m.execution_requests.to_requests();
        MergedBlockV2 {
            slot: m.slot,
            response_id: m.response_id,
            base_block_hash: m.base_block_hash,
            base_builder_pubkey: m.base_builder_pubkey,
            execution_payload: m.execution_payload,
            block_access_list: Vec::new(),
            execution_requests: requests.iter().map(|r| r.to_vec()).collect(),
            appended_blobs: m.appended_blobs,
            proposer_value: m.proposer_value,
            base_builder_revenue: m.base_builder_revenue,
            relay_revenue: m.relay_revenue,
            builder_inclusions: m.builder_inclusions,
            included_order_ids: m.included_order_ids,
            trace: m.trace,
            unmerged_txs: m.unmerged_txs,
        }
    }
}

impl TryFrom<MergedBlockV2> for MergedBlockV1 {
    type Error = alloy_rpc_types::beacon::requests::TryFromRequestsError;

    fn try_from(m: MergedBlockV2) -> Result<Self, Self::Error> {
        let requests = m.execution_requests.into_iter().map(Into::into).collect();
        let requests = alloy_eips::eip7685::Requests::new(requests);
        Ok(MergedBlockV1 {
            slot: m.slot,
            response_id: m.response_id,
            base_block_hash: m.base_block_hash,
            base_builder_pubkey: m.base_builder_pubkey,
            execution_payload: m.execution_payload,
            execution_requests: ExecutionRequestsV4::try_from(&requests)?,
            appended_blobs: m.appended_blobs,
            proposer_value: m.proposer_value,
            base_builder_revenue: m.base_builder_revenue,
            relay_revenue: m.relay_revenue,
            builder_inclusions: m.builder_inclusions,
            included_order_ids: m.included_order_ids,
            trace: m.trace,
            unmerged_txs: m.unmerged_txs,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct UnmergedTx {
    pub tx_hash: B256,
    pub reason: UnmergedReason,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, AsRefStr, EnumString)]
#[strum(serialize_all = "snake_case")]
#[ssz(enum_behaviour = "tag")]
pub enum UnmergedReason {
    ZeroPayment = 0,
    OutOfSpace = 1,
    Replaced = 2,
    Invalid = 3,
    Duplicate = 4,
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct BuilderInclusion {
    pub builder_pubkey: BlsPublicKey,
    pub origin_coinbase: Address,
    /// Value merged in from this builder's block.
    pub contribution: U256,
    /// What this builder earned for its contribution, net of payout-tx gas —
    /// matches the on-chain split.
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
    /// The builder's own current slot, not the slot of the rejected message.
    /// The relay reads this to tell whether the builder's head has moved past
    /// the slot the relay is still bidding for.
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
    /// Last tx is not a proposer payment of `block_value`.
    InvalidPayment = 3,
    /// Base fee recipient not in `builder_collaterals`.
    UnknownCollateral = 4,
    /// Failed tx validation or bad order indices in a forwarded block.
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
