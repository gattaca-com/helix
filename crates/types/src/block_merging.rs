use alloy_primitives::bytes::Bytes;
use alloy_primitives::Address;
use lh_test_random::TestRandom;
use lh_types::test_utils::TestRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum Order {
    Tx(Transaction),
    Bundle(Bundle),
}

impl TestRandom for Order {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        if rng.gen() {
            Order::Tx(Transaction::random_for_test(rng))
        } else {
            Order::Bundle(Bundle::random_for_test(rng))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    /// Index into block.transactions
    pub index: u32,
    /// If the transaction is allowed to revert
    pub can_revert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
/// All indices are taken from block.transactions.
pub struct Bundle {
    /// Signals txs that are part of the bundle
    /// and ordering of txs.
    pub txs: Vec<usize>,
    /// Txs that may revert.
    pub reverting_txs: Vec<usize>,
    /// Txs that are allowed to be omitted, but not revert.
    pub dropping_txs: Vec<usize>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct BlockMergingData {
    pub allow_appending: bool,
    pub merge_orders: Vec<Order>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
pub struct BlockMergingPreferences {
    pub allow_appending: bool,
}

/// Represents one or more transactions to be appended into a block atomically.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MergeableBundle {
    /// List of transactions that can be merged into the block.
    pub transactions: Vec<Bytes>,
    /// Txs that may revert.
    pub reverting_txs: Vec<usize>,
    /// Txs that are allowed to be omitted, but not revert.
    pub dropping_txs: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MergeableBundles {
    /// Address of the builder that submitted these bundles.
    pub origin: Address,
    /// List of mergeable bundles.
    pub bundles: Vec<MergeableBundle>,
}

impl MergeableBundles {
    pub fn new(origin: Address, bundles: Vec<MergeableBundle>) -> Self {
        Self { origin, bundles }
    }
}
