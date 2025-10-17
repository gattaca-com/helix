use std::collections::HashMap;

use alloy_eips::{Decodable2718, eip2718::Eip2718Error};
use alloy_primitives::{Address, B256, U256, bytes::Bytes};
use alloy_rpc_types::{beacon::requests::ExecutionRequestsV4, engine::ExecutionPayloadV3};
use lh_test_random::TestRandom;
use lh_types::{ForkName, test_utils::TestRandom};
use rand::Rng;
use reth_ethereum::{evm::EthEvmConfig, primitives::SignedTransaction};
use reth_node_builder::ConfigureEvm;
use reth_primitives::{NodePrimitives, Recovered};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
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

pub type SignedTx = <<EthEvmConfig as ConfigureEvm>::Primitives as NodePrimitives>::SignedTx;
pub type RecoveredTx = Recovered<SignedTx>;

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

pub type MergeableOrderBytes = MergeableOrder<Bytes>;
pub type MergeableOrderRecovered = MergeableOrder<Recovered<SignedTx>>;

/// Represents one or more transactions to be appended into a block atomically.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MergeableOrder<Tx> {
    Tx(MergeableTransaction<Tx>),
    Bundle(MergeableBundle<Tx>),
}

impl<Tx> From<MergeableTransaction<Tx>> for MergeableOrder<Tx> {
    fn from(tx: MergeableTransaction<Tx>) -> Self {
        MergeableOrder::Tx(tx)
    }
}

impl<Tx> From<MergeableBundle<Tx>> for MergeableOrder<Tx> {
    fn from(bundle: MergeableBundle<Tx>) -> Self {
        MergeableOrder::Bundle(bundle)
    }
}

impl<Tx> MergeableOrder<Tx> {
    pub fn transactions(&self) -> &[Tx] {
        match self {
            MergeableOrder::Tx(tx) => std::slice::from_ref(&tx.transaction),
            MergeableOrder::Bundle(bundle) => &bundle.transactions,
        }
    }

    pub fn into_transactions(self) -> Vec<Tx> {
        match self {
            MergeableOrder::Tx(tx) => vec![tx.transaction],
            MergeableOrder::Bundle(bundle) => bundle.transactions,
        }
    }

    pub fn reverting_txs(&self) -> &[usize] {
        match self {
            MergeableOrder::Tx(tx) if tx.can_revert => &[0],
            MergeableOrder::Tx(_) => &[],
            MergeableOrder::Bundle(bundle) => &bundle.reverting_txs,
        }
    }

    pub fn dropping_txs(&self) -> &[usize] {
        match self {
            MergeableOrder::Tx(_) => &[],
            MergeableOrder::Bundle(bundle) => &bundle.dropping_txs,
        }
    }

    pub fn origin(&self) -> &Address {
        match self {
            MergeableOrder::Tx(tx) => &tx.origin,
            MergeableOrder::Bundle(bundle) => &bundle.origin,
        }
    }
}

impl MergeableOrderBytes {
    pub fn recover(self) -> Result<MergeableOrderRecovered, RecoverError> {
        match self {
            MergeableOrder::Tx(tx) => {
                let transaction = recover_transaction(&tx.transaction)?;
                let MergeableTransaction { can_revert, origin, .. } = tx;
                Ok(MergeableOrder::Tx(MergeableTransaction { transaction, can_revert, origin }))
            }
            MergeableOrder::Bundle(bundle) => {
                let transactions = bundle
                    .transactions
                    .iter()
                    .map(recover_transaction)
                    .collect::<Result<_, _>>()?;
                let MergeableBundle { reverting_txs, dropping_txs, origin, .. } = bundle;
                Ok(MergeableOrder::Bundle(MergeableBundle {
                    transactions,
                    reverting_txs,
                    dropping_txs,
                    origin,
                }))
            }
        }
    }
}

fn recover_transaction(tx_bytes: &Bytes) -> Result<Recovered<SignedTx>, RecoverError> {
    let mut buf = tx_bytes.as_ref();
    let tx = <SignedTx as Decodable2718>::decode_2718(&mut buf)?;
    // If buffer was not fully consumed, the transaction is invalid.
    if !buf.is_empty() {
        return Err(RecoverError::TrailingData);
    }
    let recovered = tx.try_into_recovered().map_err(|_| RecoverError::InvalidSignature)?;
    Ok(recovered)
}

/// Represents a single transaction to be appended into a block atomically.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MergeableTransaction<Tx> {
    /// Transaction that can be merged into the block.
    pub transaction: Tx,
    /// Txs that may revert.
    pub can_revert: bool,
    /// Address of the builder that submitted this transaction.
    pub origin: Address,
}

/// Represents a bundle of transactions to be appended into a block atomically.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct MergeableBundle<Tx> {
    /// List of transactions that can be merged into the block.
    pub transactions: Vec<Tx>,
    /// Txs that may revert.
    pub reverting_txs: Vec<usize>,
    /// Txs that are allowed to be omitted, but not revert.
    pub dropping_txs: Vec<usize>,
    /// Address of the builder that submitted this bundle.
    pub origin: Address,
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
    pub orders: Vec<MergeableOrderBytes>,
    /// Blobs used by the orders, keyed by their versioned hash.
    pub blobs: HashMap<B256, BlobWithMetadata>,
}

impl MergeableOrders {
    pub fn new(
        origin: Address,
        orders: Vec<MergeableOrderBytes>,
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
    pub order: MergeableOrderBytes,
}

impl MergeableOrderWithOrigin {
    pub fn new(origin: Address, order: MergeableOrderBytes) -> Self {
        Self { origin, order }
    }
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockMergeRequestV1 {
    /// The original payload value
    pub original_value: U256,
    /// The address to send the proposer payment to.
    pub proposer_fee_recipient: Address,
    #[serde(with = "alloy_rpc_types::beacon::payload::beacon_payload_v3")]
    pub execution_payload: ExecutionPayloadV3,
    pub parent_beacon_block_root: B256,
    pub merging_data: Vec<MergeableOrderBytes>,
}

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMergeResponseV1 {
    #[serde(with = "alloy_rpc_types::beacon::payload::beacon_payload_v3")]
    pub execution_payload: ExecutionPayloadV3,
    pub execution_requests: ExecutionRequestsV4,
    /// Versioned hashes of the appended blob transactions.
    pub appended_blobs: Vec<B256>,
    /// Total value for the proposer
    pub proposer_value: U256,
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

#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error("decode error: {_0}")]
    Decode(#[from] Eip2718Error),
    #[error("transaction has trailing data")]
    TrailingData,
    #[error("invalid signature")]
    InvalidSignature,
}
