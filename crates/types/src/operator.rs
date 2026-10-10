use alloy_primitives::B256;
use libp2p::{Multiaddr, identity::PublicKey};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{BlsPublicKeyBytes, PayloadAndBlobs, PayloadBidData, utils};

#[derive(Debug, Decode, Encode)]
#[ssz(enum_behaviour = "union")]
pub enum OperatorMessage {
    Demotion(Demotion),
    Promotion(Promotion),
    Collateral(BuilderCollateral),
    Payload(Payload),
    Membership(CollateralMembership),
}

/// Message broadcast to operators:
/// - when a builder is demoted.
/// - when a new connection is established with another operator (every retained report, with
///   original fields and timestamps preserved).
///
/// Identity for deduplication is `(collateral_id, builder_pubkey, slot, block_hash)`. All four
/// survive a replay unchanged.
#[derive(Clone, Debug, Decode, Encode)]
pub struct Demotion {
    /// Millisecond UNIX timestamp of the demotion. Assigned when the status action is issued,
    /// not taken from the submission.
    pub ts_ms: u64,
    /// The offending submission's slot, not the detection or broadcast slot. Groups reservations.
    pub slot: u64,
    /// The pool backing the submission when it was admitted. utf8 string bytes.
    pub collateral_id: Vec<u8>,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub block_hash: B256,
    /// Full offending bid value, not a balance or incremental deduction.
    pub bid_value_wei: u128,
    /// utf8 string bytes.
    pub reason_msg: Vec<u8>,
}

/// Message broadcast to operators when a pool is promoted. Applies to every current member of
/// the pool, and clears reservations older than `ts_ms`.
#[derive(Clone, Debug, Decode, Encode)]
pub struct Promotion {
    /// Millisecond UNIX timestamp of the promotion. Assigned when the status action is issued.
    pub ts_ms: u64,
    pub slot: u64,
    /// utf8 string bytes.
    pub collateral_id: Vec<u8>,
    /// The pubkey through which promotion was requested, for alerting. Not a membership claim.
    pub builder_pubkey: BlsPublicKeyBytes,
}

/// Message broadcast to operators:
/// - when a new connection is established with another operator (one message per pool)
/// - when pool collateral is changed
///
/// Removing a contribution requires a zero-valued record; omission is not removal.
#[derive(Clone, Debug, Decode, Encode)]
pub struct BuilderCollateral {
    /// Timestamp of the message.
    pub ts_ms: u64,
    pub slot: u64,
    /// The pool this contribution backs. utf8 string bytes.
    pub collateral_id: Vec<u8>,
    /// Gross backing held by this operator group, before reservations.
    pub collateral_wei: u128,
    /// Operator group name as utf-8 bytes.
    /// If this value is set it MUST be used to deduplicate these collateral messages.
    /// Operator instances in the same group will send the same collateral amounts,
    /// which MUST NOT be summed.
    pub operator_group: Option<Vec<u8>>,
}

/// Complete membership of a pool. Never a delta. Member sets are append-only: a pubkey is never
/// removed from a pool and a retired pubkey is never reused.
#[derive(Clone, Debug, Decode, Encode)]
pub struct CollateralMembership {
    /// Timestamp of the message.
    pub ts_ms: u64,
    /// utf8 string bytes.
    pub collateral_id: Vec<u8>,
    /// The complete current set, never a delta.
    pub builder_pubkeys: Vec<BlsPublicKeyBytes>,
}

#[derive(Clone, Debug, Decode, Encode)]
pub struct Payload {
    pub ts_ms: u64,
    pub slot: u64,
    pub execution_payload: PayloadAndBlobs,
    pub proposer_pub_key: BlsPublicKeyBytes,
    pub bid_data: PayloadBidData,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Operator {
    pub name: String,
    #[serde(
        serialize_with = "utils::serialize_pubkey",
        deserialize_with = "utils::deserialize_pubkey"
    )]
    pub pubkey: PublicKey,
    pub multiaddr: Multiaddr,
    #[serde(default)]
    pub operator_group: Option<String>,
}
