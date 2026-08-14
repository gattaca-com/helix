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
    /// Operator group name as utf-8 bytes.
    /// If this value is set it MUST be used to deduplicate these collateral messages.
    /// Operator instances in the same group will send the same collateral amounts,
    /// which MUST NOT be summed.
    pub operator_group: Option<Vec<u8>>,
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
