use std::hash::{DefaultHasher, Hash, Hasher};

use alloy_primitives::B256;
use ssz_derive::{Decode, Encode};

use crate::BlsPublicKeyBytes;

pub const MSG_TYPE_DEMOTION: u8 = 0x01;
pub const MSG_TYPE_PROMOTION: u8 = 0x02;
pub const MSG_TYPE_COLLATERAL: u8 = 0x03;

pub trait HasMessageId<const TYPE: u8>: Hash {
    fn id(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        (TYPE as u64) << 56 | hasher.finish() >> 8
    }
}

/// Message broadcast to operators:
/// - when a builder is demoted.
/// - when a new connection is established with another operator (for every currently demoted
///   builder - MAY exclude the reason details and block hash, but MUST include the original
///   timestamp and slot, for correct ordering wrt any `Promotion` messages)
#[derive(Debug, Decode, Encode, Hash)]
pub struct Demotion {
    /// Originating operator, utf8 bytes
    operator_id: Vec<u8>,
    /// Millisecond UNIX timestamp of the demotion.
    ts_ms: u64,
    /// Slot number at the demotion.
    slot: u64,
    builder_pubkey: BlsPublicKeyBytes,
    block_hash: B256,
    /// utf8 string bytes.
    reason_msg: Vec<u8>,
}

impl HasMessageId<MSG_TYPE_DEMOTION> for Demotion {}

/// Message broadcast to operators when a builder is promoted.
#[derive(Debug, Decode, Encode, Hash)]
pub struct Promotion {
    /// Originating operator, utf8 bytes
    operator_id: Vec<u8>,
    /// Millisecond UNIX timestamp of the promotion.
    ts_ms: u64,
    slot: u64,
    builder_pubkey: BlsPublicKeyBytes,
}

impl HasMessageId<MSG_TYPE_PROMOTION> for Demotion {}

/// Message broadcast to operators:
/// - when a new connection is established with another operator (one message for every builder)
/// - when builder collateral is changed
/// Note that total collateral held by the operator is broadcast.
#[derive(Debug, Decode, Encode, Hash)]
pub struct BuilderCollateral {
    /// Originating operator, utf8 bytes
    operator_id: Vec<u8>,
    /// Timestamp of the message.
    ts_ms: u64,
    slot: u64,
    builder_pubkey: BlsPublicKeyBytes,
    /// The TOTAL builder collateral held by the operator. Each operator
    /// will need to track amounts held at other operators and sum.
    collateral_wei: u128,
}

impl HasMessageId<MSG_TYPE_COLLATERAL> for Demotion {}
