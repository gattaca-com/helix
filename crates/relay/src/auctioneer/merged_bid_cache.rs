use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_consensus::{Bytes48, Transaction, TxEnvelope, TxType};
use alloy_primitives::{B256, Bytes};
use alloy_rlp::Decodable;
use helix_common::{RelayConfig, metrics::MERGE_TRACE_LATENCY, utils::utcnow_ms};
use helix_types::{
    BlobWithMetadata, BlobsBundle, BlockMergingData, BundleOrder, KzgCommitment, MergeableBundle,
    MergeableOrder, MergeableOrders, MergeableTransaction, Order, Transactions,
};
use tracing::{debug, trace, warn};

use crate::auctioneer::types::PayloadEntry;

#[derive(thiserror::Error, Debug)]
pub enum OrderValidationError {
    #[error("invalid block merging tx index, got {got} with a tx count of {len}")]
    InvalidTxIndex { got: usize, len: usize },
    #[error("blob transaction does not reference any blobs")]
    EmptyBlobTransaction,
    #[error("blob transaction references blobs not in the block")]
    MissingBlobs,
    #[error("flagged indices reference tx outside of bundle")]
    FlaggedIndicesOutOfBounds,
    #[error("transaction decode failure")]
    FailedToDecode(#[from] alloy_rlp::Error),
    #[error("zero value transaction")]
    ZeroValue,
}

pub struct MergedBidCache {
    curr_bid_slot: u64,
    config: RelayConfig,
    best_merged_block: Option<BestMergedBlock>,
}

impl MergedBidCache {
    pub fn new(config: RelayConfig) -> Self {
        Self { curr_bid_slot: 0, config, best_merged_block: None }
    }

    pub fn on_new_slot(&mut self, bid_slot: u64) {
        self.curr_bid_slot = bid_slot;
        self.best_merged_block = None;
    }

    pub fn store_merged(&mut self, entry: PayloadEntry, base_block_time_ms: u64) {
        self.best_merged_block = Some(BestMergedBlock { base_block_time_ms, bid: entry });
    }

    pub fn get_header(&self, original_bid: &PayloadEntry) -> Option<PayloadEntry> {
        trace!("fetching merged header");
        let start_time = Instant::now();
        let entry = self.best_merged_block.as_ref()?;

        if entry.bid.value() <= original_bid.value() {
            trace!("merged bid not higher");
            return None;
        }

        let now_ms = utcnow_ms();
        if entry.base_block_time_ms <
            now_ms.saturating_sub(self.config.block_merging_config.max_merged_bid_age_ms)
        {
            trace!("merged bid stale");
            return None;
        }

        if entry.bid.parent_hash() != original_bid.parent_hash() {
            trace!("merged bid parent hash mismatch");
            return None;
        }

        record_step("get_header", start_time.elapsed());

        if self.config.block_merging_config.is_dry_run {
            trace!("dry run: not returning merged header");
            return None;
        }

        Some(entry.bid.clone())
    }
}

struct BestMergedBlock {
    base_block_time_ms: u64,
    bid: PayloadEntry,
}

pub fn get_mergeable_orders(
    payload: &helix_types::SignedBidSubmission,
    merging_data: &BlockMergingData,
) -> Result<MergeableOrders, OrderValidationError> {
    let start_time = Instant::now();
    let execution_payload = payload.execution_payload_ref();
    let block_blobs_bundles = payload.blobs_bundle();
    let blob_versioned_hashes: Vec<_> =
        block_blobs_bundles.commitments.iter().map(|c| calculate_versioned_hash(*c)).collect();
    let txs = &execution_payload.transactions;

    let mergeable_orders = merging_data
        .merge_orders
        .as_slice()
        .iter()
        .filter_map(|order| match order_to_mergeable(order, txs, &blob_versioned_hashes) {
            Err(err) => {
                debug!(?order, ?err, "dropping invalid order during mergeable orders extraction");
                None
            }
            other => Some(other),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let blobs = blobs_bundle_to_hashmap(blob_versioned_hashes, &block_blobs_bundles);
    record_step("get_mergeable_orders", start_time.elapsed());

    Ok(MergeableOrders::new(merging_data.builder_address, mergeable_orders, blobs))
}

fn order_to_mergeable(
    order: &Order,
    txs: &Transactions,
    blob_versioned_hashes: &[B256],
) -> Result<MergeableOrder, OrderValidationError> {
    match order {
        Order::Tx(tx) => {
            let Some(raw_tx) = txs.get(tx.index) else {
                return Err(OrderValidationError::InvalidTxIndex { got: tx.index, len: txs.len() });
            };
            if is_blob_transaction(raw_tx) {
                trace!(raw_tx = ?raw_tx, "validating blob transaction in order");
                validate_blobs(raw_tx, blob_versioned_hashes)?;
            }
            validate_builder_payment(raw_tx)?;
            let mergeable_tx =
                MergeableTransaction { transaction: raw_tx.0.clone(), can_revert: tx.can_revert }
                    .into();
            Ok(mergeable_tx)
        }
        Order::Bundle(bundle) => {
            bundle.validate().map_err(|_| OrderValidationError::FlaggedIndicesOutOfBounds)?;
            let transactions = bundle
                .txs
                .iter()
                .map(|tx_index| {
                    let Some(raw_tx) = txs.get(*tx_index) else {
                        return Err(OrderValidationError::InvalidTxIndex {
                            got: *tx_index,
                            len: txs.len(),
                        });
                    };
                    if is_blob_transaction(raw_tx) {
                        trace!(raw_tx = ?raw_tx, "validating blob transaction in bundle order");
                        validate_blobs(raw_tx, blob_versioned_hashes)?;
                    }
                    Ok(raw_tx.0.clone())
                })
                .collect::<Result<_, OrderValidationError>>()?;
            let BundleOrder { reverting_txs, dropping_txs, .. } = bundle;
            let mergeable_bundle = MergeableBundle {
                transactions,
                reverting_txs: reverting_txs.clone(),
                dropping_txs: dropping_txs.clone(),
            }
            .into();
            Ok(mergeable_bundle)
        }
    }
}

fn validate_blobs(
    raw_tx: &[u8],
    blob_versioned_hashes: &[B256],
) -> Result<(), OrderValidationError> {
    let versioned_hashes = get_tx_versioned_hashes(raw_tx);
    let num_blobs = versioned_hashes.len();
    if num_blobs == 0 {
        return Err(OrderValidationError::EmptyBlobTransaction);
    }
    if versioned_hashes.iter().any(|h| !blob_versioned_hashes.iter().any(|vh| vh == h)) {
        debug!(%num_blobs, "blob tx refs blobs not in the block");
        return Err(OrderValidationError::MissingBlobs);
    }
    Ok(())
}

fn validate_builder_payment(mut raw_tx: &[u8]) -> Result<(), OrderValidationError> {
    let tx = TxEnvelope::decode(&mut raw_tx)?;
    if tx.priority_fee_or_price() == 0 && tx.input().is_empty() {
        debug!(?tx, "zero value transaction");
        Err(OrderValidationError::ZeroValue)
    } else {
        Ok(())
    }
}

fn blobs_bundle_to_hashmap(
    blob_versioned_hashes: Vec<B256>,
    bundle: &BlobsBundle,
) -> HashMap<B256, BlobWithMetadata> {
    blob_versioned_hashes
        .into_iter()
        .zip(bundle.iter_blobs())
        .map(|(versioned_hash, (blob, commitment, proofs))| {
            (versioned_hash, BlobWithMetadata {
                commitment: *commitment,
                proofs: proofs.to_vec(),
                blob: blob.clone(),
            })
        })
        .collect()
}

fn is_blob_transaction(raw_tx: &Bytes) -> bool {
    raw_tx.first().is_some_and(|&b| b == TxType::Eip4844)
}

fn get_tx_versioned_hashes(mut raw_tx: &[u8]) -> Vec<B256> {
    let tx = match TxEnvelope::decode(&mut raw_tx) {
        Ok(tx) => tx,
        Err(err) => {
            warn!(?err, "failed to decode transaction for versioned hash extraction");
            return vec![];
        }
    };
    match tx {
        TxEnvelope::Eip4844(tx_eip4844) => {
            tx_eip4844.blob_versioned_hashes().map(|v| v.to_vec()).unwrap_or_default()
        }
        _ => vec![],
    }
}

fn calculate_versioned_hash(commitment: Bytes48) -> B256 {
    KzgCommitment(*commitment).calculate_versioned_hash()
}

pub fn record_step(label: &str, duration: Duration) {
    MERGE_TRACE_LATENCY.with_label_values(&[label]).observe(duration.as_nanos() as f64 / 1000.0);
}
