use std::{collections::HashMap, hash::Hash};

use alloy_primitives::{bytes::Bytes, Address, B256, U256};
use lh_test_random::TestRandom;
use lh_types::{test_utils::TestRandom, ForkName};
use rand::Rng;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use ssz_derive::{Decode, Encode};

use crate::{
    fields::{KzgCommitment, KzgProof},
    Blob, SignedBidSubmission,
};

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignedBidSubmissionWithMergingData {
    pub submission: SignedBidSubmission,
    pub merging_data: BlockMergingData,
}

impl SignedBidSubmissionWithMergingData {
    pub fn maybe_upgrade_to_fulu(self, fork: ForkName) -> Self {
        Self {
            submission: self.submission.maybe_upgrade_to_fulu(fork),
            merging_data: self.merging_data,
        }
    }
}

// FIXME: panics at runtime
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum Order {
    Tx(TransactionOrder),
    Bundle(BundleOrder),
}

impl TestRandom for Order {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        if rng.random() {
            Order::Tx(TransactionOrder::random_for_test(rng))
        } else {
            Order::Bundle(BundleOrder::random_for_test(rng))
        }
    }
}

/// Vector of transaction indices. Aliased for easier use.
pub type TxIndices = SmallVec<[usize; 8]>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
pub struct TransactionOrder {
    /// Index into block.transactions
    pub index: usize,
    /// If the transaction is allowed to revert
    pub can_revert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom)]
#[serde(deny_unknown_fields)]
/// References a bundle of transactions via their indices.
/// All indices are for the block the transactions come from.
pub struct BundleOrder {
    /// Signals txs that are part of the bundle and ordering of txs.
    /// Indices are for the block body the transactions come from.
    pub txs: TxIndices,
    /// Txs that may revert.
    /// Indices are for the [txs](Self::txs) array.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    /// Indices are for the [txs](Self::txs) array.
    pub dropping_txs: TxIndices,
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidTxIndex;

impl BundleOrder {
    pub fn validate(&self) -> Result<(), InvalidTxIndex> {
        if self.reverting_txs.iter().any(|&i| i >= self.txs.len()) ||
            self.dropping_txs.iter().any(|&i| i >= self.txs.len())
        {
            return Err(InvalidTxIndex);
        }
        Ok(())
    }
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
    Debug, Default, PartialEq, Eq, Clone, Copy, Serialize, Deserialize, Encode, Decode, TestRandom,
)]
pub struct BlockMergingPreferences {
    pub allow_appending: bool,
}

/// Represents one or more transactions to be appended into a block atomically.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Hash)]
#[serde(untagged)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct MergeableTransaction {
    /// Transaction that can be merged into the block.
    pub transaction: Bytes,
    /// If the transaction may revert.
    pub can_revert: bool,
}

/// Represents a bundle of transactions to be appended into a block atomically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct MergeableBundle {
    /// List of transactions that can be merged into the block.
    pub transactions: Vec<Bytes>,
    /// Txs that may revert.
    /// Indices are for the [transactions](Self::transactions) array.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    /// Indices are for the [transactions](Self::transactions) array.
    pub dropping_txs: TxIndices,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum BlobWithMetadata {
    V1(BlobWithMetadataV1),
    V2(BlobWithMetadataV2),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlobWithMetadataV1 {
    pub commitment: KzgCommitment,
    pub proof: KzgProof,
    pub blob: Blob,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlobWithMetadataV2 {
    pub commitment: KzgCommitment,
    pub proofs: Vec<KzgProof>,
    pub blob: Blob,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableOrders {
    /// Address of the builder that submitted these orders.
    pub origin: Address,
    /// List of mergeable orders.
    pub orders: Vec<MergeableOrder>,
    /// Blobs used by the orders, keyed by their versioned hash.
    pub blobs: HashMap<B256, BlobWithMetadata>,
}

impl MergeableOrders {
    pub fn new(
        origin: Address,
        orders: Vec<MergeableOrder>,
        blobs: HashMap<B256, BlobWithMetadata>,
    ) -> Self {
        Self { origin, orders, blobs }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableOrderWithOrigin {
    /// Address of the builder that submitted this order.
    pub origin: Address,
    /// Mergeable order.
    #[serde(flatten)]
    pub order: MergeableOrder,
}

impl MergeableOrderWithOrigin {
    pub fn new(origin: Address, order: MergeableOrder) -> Self {
        Self { origin, order }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuilderInclusionResult {
    pub revenue: U256,
    pub tx_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct MergedBlock {
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub original_value: U256,
    pub merged_value: U256,
    pub original_tx_count: usize,
    pub merged_tx_count: usize,
    pub original_blob_count: usize,
    pub merged_blob_count: usize,
    pub builder_inclusions: HashMap<Address, BuilderInclusionResult>,
}

impl MergedBlock {
    pub fn block_hash(&self) -> B256 {
        self.block_hash
    }

    pub fn to_alert_message(&self) -> String {
        let builder_inclusions = self
            .builder_inclusions
            .iter()
            .map(|(address, res)| format!("*{}*: {}", res.tx_count, address,))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "📦 *Merged Block Delivered*\n\
                \n\
                *Slot:* `{}`\n\
                *Block Number:* `{}`\n\
                *Block Hash:* `{}`\n\
                *Value:* `{}` → `{}`\n\
                *Transactions:* `{}` → `{}`\n\
                *Blobs:* `{}` → `{}`\n\
                \n\
                ━━━━━━━━━━━━━━━\n\
                *Merged txs by builder:*\n{}\n\
                ━━━━━━━━━━━━━━━\n",
            self.slot,
            self.block_number,
            self.block_hash,
            self.original_value,
            self.merged_value,
            self.original_tx_count,
            self.merged_tx_count,
            self.original_blob_count,
            self.merged_blob_count,
            builder_inclusions
        )
    }
}
