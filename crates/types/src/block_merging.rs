use std::hash::Hash;

use alloy_primitives::{bytes::Bytes, Address};
use lh_test_random::TestRandom;
use lh_types::test_utils::TestRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use ssz_derive::{Decode, Encode};

use crate::{BlobsBundle, SignedBidSubmission};

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignedBidSubmissionWithMergingData {
    pub submission: SignedBidSubmission,
    pub merging_data: BlockMergingData,
}

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

/// Vector of transaction indices. Aliased for easier use.
pub type TxIndices = SmallVec<[usize; 2]>;

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    /// Index into block.transactions
    pub index: usize,
    /// If the transaction is allowed to revert
    pub can_revert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
/// All indices are taken from block.transactions.
pub struct Bundle {
    /// Signals txs that are part of the bundle
    /// and ordering of txs.
    pub txs: TxIndices,
    /// Txs that may revert.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    pub dropping_txs: TxIndices,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct BlockMergingData {
    pub allow_appending: bool,
    pub builder_address: Address,
    pub merge_orders: Vec<Order>,
}

impl BlockMergingData {
    pub fn is_default(&self) -> bool {
        !self.allow_appending && self.merge_orders.is_empty()
    }
}

#[derive(
    Debug, Default, PartialEq, Eq, Clone, Serialize, Deserialize, Encode, Decode, TestRandom,
)]
pub struct BlockMergingPreferences {
    pub allow_appending: bool,
}

/// Represents one or more transactions to be appended into a block atomically.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Hash)]
pub enum MergeableOrder {
    Tx(MergeableTransaction),
    Bundle(MergeableBundle),
}

impl From<MergeableTransaction> for MergeableOrder {
    fn from(tx: MergeableTransaction) -> Self {
        MergeableOrder::Tx(tx)
    }
}

impl From<MergeableBundle> for MergeableOrder {
    fn from(bundle: MergeableBundle) -> Self {
        MergeableOrder::Bundle(bundle)
    }
}

/// Represents a single transaction to be appended into a block atomically.
/// Note that [`PartialEq`] and [`Hash`] implementations ignore the blobs field.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableTransaction {
    /// Transaction that can be merged into the block.
    pub transaction: Bytes,
    /// Txs that may revert.
    pub can_revert: bool,
    /// Blobs used by the transaction
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Eq for MergeableTransaction {}

impl PartialEq for MergeableTransaction {
    fn eq(&self, other: &Self) -> bool {
        self.transaction == other.transaction && self.can_revert == other.can_revert
    }
}

impl Hash for MergeableTransaction {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.transaction.hash(state);
        self.can_revert.hash(state);
    }
}

/// Represents a bundle of transactions to be appended into a block atomically.
/// Note that [`PartialEq`] and [`Hash`] implementations ignore the blobs field.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableBundle {
    /// List of transactions that can be merged into the block.
    pub transactions: Vec<Bytes>,
    /// Txs that may revert.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    pub dropping_txs: TxIndices,
    /// Blobs used by the bundle
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Eq for MergeableBundle {}

impl PartialEq for MergeableBundle {
    fn eq(&self, other: &Self) -> bool {
        self.transactions == other.transactions &&
            self.reverting_txs == other.reverting_txs &&
            self.dropping_txs == other.dropping_txs
    }
}

impl Hash for MergeableBundle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.transactions.hash(state);
        self.reverting_txs.hash(state);
        self.dropping_txs.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MergeableOrders {
    /// Address of the builder that submitted these orders.
    pub origin: Address,
    /// List of mergeable orders.
    pub orders: Vec<MergeableOrder>,
}

impl MergeableOrders {
    pub fn new(origin: Address, orders: Vec<MergeableOrder>) -> Self {
        Self { origin, orders }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableOrderWithOrigin {
    /// Address of the builder that submitted this order.
    pub origin: Address,
    /// Mergeable order.
    pub order: MergeableOrder,
}

impl MergeableOrderWithOrigin {
    pub fn new(origin: Address, order: MergeableOrder) -> Self {
        Self { origin, order }
    }
}
