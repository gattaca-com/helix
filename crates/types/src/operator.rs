use alloy_primitives::B256;
use ssz_derive::{Decode, Encode};

use crate::BlsPublicKeyBytes;

#[derive(Debug, Decode, Encode)]
#[ssz(enum_behaviour = "transparent")]
pub enum OperatorMessage {
    Demotion(Demotion),
    Promotion(Promotion),
    Collateral(BuilderCollateral),
}

/// Message broadcast to operators:
/// - when a builder is demoted.
/// - when a new connection is established with another operator (for every currently demoted
///   builder - MAY exclude the reason details and block hash, but MUST include the original
///   timestamp and slot, for correct ordering wrt any `Promotion` messages)
#[derive(Clone, Debug, Decode, Encode)]
pub struct Demotion {
    /// Millisecond UNIX timestamp of the demotion.
    pub ts_ms: u64,
    /// Slot number at the demotion.
    pub slot: u64,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub block_hash: B256,
    /// utf8 string bytes.
    pub reason_msg: Vec<u8>,
}

/// Message broadcast to operators when a builder is promoted.
#[derive(Clone, Debug, Decode, Encode)]
pub struct Promotion {
    /// Millisecond UNIX timestamp of the promotion.
    pub ts_ms: u64,
    pub slot: u64,
    pub builder_pubkey: BlsPublicKeyBytes,
}

/// Message broadcast to operators:
/// - when a new connection is established with another operator (one message for every builder)
/// - when builder collateral is changed
/// Note that total collateral held by the operator is broadcast.
#[derive(Clone, Debug, Decode, Encode)]
pub struct BuilderCollateral {
    /// Timestamp of the message.
    pub ts_ms: u64,
    pub slot: u64,
    pub builder_pubkey: BlsPublicKeyBytes,
    /// The TOTAL builder collateral held by the operator. Each operator
    /// will need to track amounts held at other operators and sum.
    pub collateral_wei: u128,
}
