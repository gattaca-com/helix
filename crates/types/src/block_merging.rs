use std::{collections::HashMap, hash::Hash};

use alloy_primitives::{Address, B256, Bytes, U256};
use lh_test_random::TestRandom;
use lh_types::{ForkName, test_utils::TestRandom};
use rand::Rng;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use ssz_derive::{Decode, Encode};

use crate::{
    Blob, SignedBidSubmission,
    fields::{KzgCommitment, KzgProof},
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

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[ssz(enum_behaviour = "union")]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Encode, Decode, TestRandom, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransactionOrder {
    /// Index into block.transactions
    pub index: usize,
    /// If the transaction is allowed to revert
    pub can_revert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TestRandom, PartialEq, Eq)]
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

#[derive(
    Debug, Default, Clone, Serialize, Deserialize, Encode, Decode, TestRandom, PartialEq, Eq,
)]
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

pub struct MergeableOrdersWithPref {
    pub allow_appending: bool,
    pub orders: MergeableOrders,
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

#[cfg(test)]
mod tests {
    use ssz::{Decode, Encode};

    use super::*;

    #[test]
    fn order_ssz_round_trip_tx() {
        let tx_order = TransactionOrder { index: 42, can_revert: true };
        let order = Order::Tx(tx_order);

        let bytes = order.as_ssz_bytes();
        let decoded = Order::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn order_ssz_round_trip_bundle() {
        let bundle_order = BundleOrder {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
        };
        let order = Order::Bundle(bundle_order);

        let bytes = order.as_ssz_bytes();
        let decoded = Order::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn block_merging_data_ssz_round_trip_mixed_orders() {
        let tx_order = Order::Tx(TransactionOrder { index: 1, can_revert: false });
        let bundle_order = Order::Bundle(BundleOrder {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
        });

        let data = BlockMergingData {
            allow_appending: true,
            builder_address: Address::repeat_byte(0x42),
            merge_orders: vec![tx_order, bundle_order],
        };

        let bytes = data.as_ssz_bytes();
        let decoded = BlockMergingData::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(data, decoded);
    }

    #[test]
    fn order_json_round_trip_tx() {
        let tx_order = TransactionOrder { index: 42, can_revert: true };
        let order = Order::Tx(tx_order);

        let json = serde_json::to_string(&order).expect("JSON encode should succeed");
        let decoded: Order = serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn order_json_round_trip_bundle() {
        let bundle_order = BundleOrder {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
        };
        let order = Order::Bundle(bundle_order);

        let json = serde_json::to_string(&order).expect("JSON encode should succeed");
        let decoded: Order = serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn block_merging_data_json_round_trip_mixed_orders() {
        let tx_order = Order::Tx(TransactionOrder { index: 1, can_revert: false });
        let bundle_order = Order::Bundle(BundleOrder {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
        });

        let data = BlockMergingData {
            allow_appending: true,
            builder_address: Address::repeat_byte(0x42),
            merge_orders: vec![tx_order, bundle_order],
        };

        let json = serde_json::to_string(&data).expect("JSON encode should succeed");
        let decoded: BlockMergingData =
            serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(data, decoded);
    }
}
