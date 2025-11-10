use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc, time::{Duration, Instant},
};

use alloy_consensus::{Bytes48, TxEip4844, TxType};
use alloy_primitives::{B256, U256};
use bytes::Bytes;
use helix_common::{RelayConfig, chain_info::ChainInfo, local_cache::LocalCache, metrics::MERGE_TRACE_LATENCY, utils::utcnow_ms};
use helix_types::{
    BlobWithMetadata, BlobWithMetadataV1, BlobWithMetadataV2, BlobsBundle, BlobsBundleVersion,
    BlockMergingData, BundleOrder, KzgCommitment, MergeableBundle, MergeableOrder,
    MergeableOrderWithOrigin, MergeableOrders, MergeableOrdersWithPref, MergeableTransaction,
    MergedBlock, Order, PayloadAndBlobs, SignedBidSubmission, Transactions,
};
use tracing::{debug, error, trace, warn};

use crate::auctioneer::{
    BlockMergeResponse, PayloadBidData, PayloadHeaderData, types::PayloadEntry,
};

const MERGE_REQUEST_INTERVAL_MS: u64 = 50;

type BlockHash = B256;

#[derive(Debug, thiserror::Error)]
pub enum PayloadMergingError {
    #[error("could not fetch original payload")]
    CouldNotFetchOriginalPayload,
    #[error(
        "merged payload value is lower or equal to original bid. original: {original}, merged: {merged}"
    )]
    MergedPayloadNotValuable { original: U256, merged: U256 },
    #[error("blob not found")]
    BlobNotFound,
    #[error("reached maximum blob count for block")]
    MaxBlobCountReached,
}

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
}

pub struct BlockMerger {
    curr_bid_slot: u64,
    config: RelayConfig,
    chain_info: ChainInfo,
    local_cache: LocalCache,
    best_merged_block: Option<BestMergedBlock>,
    best_mergeable_orders: BestMergeableOrders,
    base_blocks: HashMap<BlockHash, BaseBlockData>,
    has_new_base_block: bool,
    last_merge_request_time_ms: u64,
}

impl BlockMerger {
    pub fn new(
        curr_bid_slot: u64,
        chain_info: ChainInfo,
        local_cache: LocalCache,
        config: RelayConfig,
    ) -> Self {
        Self {
            curr_bid_slot,
            config,
            chain_info,
            local_cache,
            best_merged_block: None,
            best_mergeable_orders: BestMergeableOrders::new(),
            base_blocks: HashMap::new(),
            has_new_base_block: false,
            last_merge_request_time_ms: 0,
        }
    }
}

impl BlockMerger {
    pub fn on_new_slot(&mut self, bid_slot: u64) {
        self.curr_bid_slot = bid_slot;
        self.best_merged_block = None;
        self.best_mergeable_orders.reset();
        self.base_blocks.clear();
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = 0;
    }

    pub fn get_header(&self, original_bid: &PayloadHeaderData) -> Option<PayloadHeaderData> {
        let start_time = Instant::now();
        let entry = self.best_merged_block.as_ref()?;
        if !merged_bid_higher(
            &entry.bid,
            original_bid,
            entry.base_block_time_ms,
            self.config.block_merging_config.max_merged_bid_age_ms,
        ) {
            return None;
        }

        if entry.bid.payload_and_blobs.execution_payload.parent_hash !=
            original_bid.payload_and_blobs.execution_payload.parent_hash
        {
            return None;
        }

        record_step("get_header", start_time.elapsed());

        Some(entry.bid.clone())
    }

    pub fn update(
        &mut self,
        is_top_bid: bool,
        block_hash: &B256,
        block_value: U256,
        merging_data: MergeableOrdersWithPref,
    ) {
        trace!(is_top_bid, merging_data.allow_appending, %block_hash,"updating block merger state");

        if !merging_data.orders.orders.is_empty() {
            self.best_mergeable_orders.insert_orders(block_value, merging_data.orders);
        }

        if is_top_bid && merging_data.allow_appending {
            self.update_base_block(block_hash, block_value);
        }
    }

    pub fn should_request_merge(&self) -> bool {
        let start_time = Instant::now();
        let has_new_data = self.best_mergeable_orders.has_new_orders() ||
            (self.best_mergeable_orders.has_orders() && self.has_new_base_block);
        if !has_new_data {
            return false;
        }
        let res = utcnow_ms().saturating_sub(self.last_merge_request_time_ms) >= MERGE_REQUEST_INTERVAL_MS;
        record_step("should_request_merge", start_time.elapsed());
        res
    }

    pub fn fetch_best_mergeable_orders(
        &mut self,
    ) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        let start_time = Instant::now();
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = utcnow_ms();
        let res = self.best_mergeable_orders.load();
        record_step("fetch_best_mergeable_orders", start_time.elapsed());
        res
    }

    pub fn prepare_merged_payload_for_storage(
        &mut self,
        response: BlockMergeResponse,
        original_payload: Arc<PayloadAndBlobs>,
    ) -> Result<PayloadEntry, PayloadMergingError> {
        let start_time = Instant::now();
        let bid_slot = self.curr_bid_slot;
        let max_blobs_per_block = self.chain_info.max_blobs_per_block();

        let base_block_data = match self.base_blocks.get(&response.base_block_hash) {
            Some(data) => data,
            None => {
                warn!(%response.base_block_hash, "could not fetch original payload for merged block");
                return Err(PayloadMergingError::CouldNotFetchOriginalPayload);
            }
        };

        if response.proposer_value <= base_block_data.value {
            warn!(
                original = %base_block_data.value,
                merged = %response.proposer_value,
                "merged payload value is not higher than original bid"
            );
            return Err(PayloadMergingError::MergedPayloadNotValuable {
                original: base_block_data.value,
                merged: response.proposer_value,
            });
        }

        let block_hash = response.execution_payload.block_hash;

        self.local_cache.save_merged_block(MergedBlock {
            slot: bid_slot,
            block_number: response.execution_payload.block_number,
            block_hash,
            original_value: base_block_data.value,
            merged_value: response.proposer_value,
            original_tx_count: original_payload.execution_payload.transactions.len(),
            merged_tx_count: response.execution_payload.transactions.len(),
            original_blob_count: original_payload.blobs_bundle.blobs().len(),
            merged_blob_count: original_payload.blobs_bundle.blobs().len() +
                response.appended_blobs.len(),
            builder_inclusions: response.builder_inclusions,
        });

        let blobs = &self.best_mergeable_orders.mergeable_blob_bundles;

        let mut merged_blobs_bundle = original_payload.blobs_bundle.clone();
        append_merged_blobs(
            &mut merged_blobs_bundle,
            blobs,
            &response.appended_blobs,
            max_blobs_per_block,
        )?;

        let withdrawals_root = response.execution_payload.withdrawals_root();

        let payload_and_blobs = Arc::new(PayloadAndBlobs {
            execution_payload: response.execution_payload,
            blobs_bundle: merged_blobs_bundle,
        });

        let bid_data = PayloadBidData {
            withdrawals_root,
            execution_requests: Arc::new(response.execution_requests),
            value: response.proposer_value,
            tx_root: None,
        };

        let new_bid = PayloadHeaderData {
            payload_and_blobs: payload_and_blobs.clone(),
            bid_data: bid_data.clone(),
        };

        // Store locally to serve header requests
        self.best_merged_block =
            Some(BestMergedBlock { base_block_time_ms: base_block_data.time_ms, bid: new_bid });

        record_step("prepare_merged_payload_for_storage", start_time.elapsed());

        // Return the payload entry to be stored for get payload calls
        Ok(PayloadEntry { payload_and_blobs, bid_data: Some(bid_data) })
    }

    fn update_base_block(&mut self, base_block_hash: &B256, base_block_value: U256) {
        if self.base_blocks.contains_key(base_block_hash) {
            return;
        }

        let start_time = Instant::now();

        let base_block_time_ms = utcnow_ms();

        let base_block_data =
            BaseBlockData { time_ms: base_block_time_ms, value: base_block_value };

        self.base_blocks.insert(*base_block_hash, base_block_data);
        self.has_new_base_block = true;
        record_step("update_base_block", start_time.elapsed());
    }
}

struct BaseBlockData {
    time_ms: u64,
    value: U256,
}

struct BestMergedBlock {
    base_block_time_ms: u64,
    bid: PayloadHeaderData,
}

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    index: usize,
}

#[derive(Debug, Clone)]
struct BestMergeableOrders {
    order_map: HashMap<MergeableOrder, OrderMetadata>,
    best_orders: Vec<MergeableOrderWithOrigin>,
    mergeable_blob_bundles: HashMap<B256, BlobWithMetadata>,
    has_new_orders: bool,
}

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    fn new() -> Self {
        Self {
            order_map: HashMap::with_capacity(5000),
            mergeable_blob_bundles: HashMap::with_capacity(5000),
            best_orders: Vec::with_capacity(5000),
            has_new_orders: false,
        }
    }

    fn load(&mut self) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        // Reset the new orders flag
        self.has_new_orders = false;
        Some((&self.best_orders, &self.mergeable_blob_bundles))
    }

    fn has_new_orders(&self) -> bool {
        self.has_new_orders && !self.best_orders.is_empty()
    }

    fn has_orders(&self) -> bool {
        !self.best_orders.is_empty()
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    fn insert_orders(&mut self, bid_value: U256, mergeable_orders: MergeableOrders) {
        let start_time = Instant::now();
        let origin = mergeable_orders.origin;
        // Insert each order into the order map
        mergeable_orders.orders.into_iter().for_each(|o| {
            // Collect all blobs for this order
            match self.order_map.entry(o) {
                // If the order already exists, keep the one with the highest bid
                Entry::Occupied(mut e) => {
                    if e.get().value < bid_value {
                        // We update the value of the bid to the highest
                        e.get_mut().value = bid_value;
                        // and the origin for the order
                        self.best_orders[e.get().index].origin = origin;
                    }
                }
                // Otherwise, insert the new order
                Entry::Vacant(e) => {
                    // If there were any new orders, we mark it
                    self.has_new_orders = true;

                    let index = self.best_orders.len();
                    // We insert the order to our list of orders
                    self.best_orders.push(MergeableOrderWithOrigin::new(origin, e.key().clone()));
                    // and insert the metadata
                    e.insert(OrderMetadata { value: bid_value, index });
                }
            }
        });
        // Insert new blobs
        self.mergeable_blob_bundles.extend(mergeable_orders.blobs);
        record_step("insert_orders", start_time.elapsed());
    }

    pub fn reset(&mut self) {
        self.order_map.clear();
        self.best_orders.clear();
        self.mergeable_blob_bundles.clear();
    }
}

/// Expands the references in [`BlockMergingData`] from the transactions in the
/// payload of the given submission. If any bundle references a transaction not in
/// the payload, it will be silently ignored.
pub fn get_mergeable_orders(
    payload: &SignedBidSubmission,
    merging_data: &BlockMergingData,
) -> Result<MergeableOrders, OrderValidationError> {
    let start_time = Instant::now();
    let execution_payload = payload.execution_payload_ref();
    let block_blobs_bundles = payload.blobs_bundle();
    let blob_versioned_hashes: Vec<_> =
        block_blobs_bundles.commitments().iter().map(|c| calculate_versioned_hash(*c)).collect();
    let txs = &execution_payload.transactions;

    // Expand all orders to include the tx's bytes, checking for missing blobs.
    let mergeable_orders = merging_data
        .merge_orders
        .as_slice()
        .iter()
        .map(|order| order_to_mergeable(order, txs, &blob_versioned_hashes))
        .collect::<Result<Vec<_>, _>>()?;

    // Stores all block blobs inside a map keyed by versioned hash
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
                // If the tx references bundles not in the block, we drop it
                validate_blobs(raw_tx, blob_versioned_hashes)?;
            }

            let transaction = Bytes::from(raw_tx.to_vec());
            let mergeable_tx =
                MergeableTransaction { transaction, can_revert: tx.can_revert }.into();
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
                        // If the tx references bundles not in the block, we drop the bundle
                        validate_blobs(raw_tx, blob_versioned_hashes)?;
                    }

                    Ok(Bytes::from_owner(raw_tx.to_vec()))
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
    let mut missing_blobs =
        versioned_hashes.iter().map(|h| !blob_versioned_hashes.iter().any(|vh| vh == h));
    if missing_blobs.any(|f| f) {
        return Err(OrderValidationError::MissingBlobs);
    }
    Ok(())
}

fn blobs_bundle_to_hashmap(
    blob_versioned_hashes: Vec<B256>,
    bundle: &BlobsBundle,
) -> HashMap<B256, BlobWithMetadata> {
    let version = bundle.version();
    blob_versioned_hashes
        .into_iter()
        .zip(bundle.iter_blobs())
        .map(|(versioned_hash, (blob, commitment, proofs))| match version {
            BlobsBundleVersion::V1 => (
                versioned_hash,
                BlobWithMetadata::V1(BlobWithMetadataV1 {
                    commitment: *commitment,
                    proof: proofs[0],
                    blob: blob.clone(),
                }),
            ),
            BlobsBundleVersion::V2 => (
                versioned_hash,
                BlobWithMetadata::V2(BlobWithMetadataV2 {
                    commitment: *commitment,
                    proofs: proofs.to_vec(),
                    blob: blob.clone(),
                }),
            ),
        })
        .collect()
}

fn is_blob_transaction(raw_tx: &[u8]) -> bool {
    // First byte is always the transaction type, or >= 0xc0 for legacy
    // (source: https://eips.ethereum.org/EIPS/eip-2718)
    raw_tx.first().is_some_and(|&b| b == TxType::Eip4844)
}

fn get_tx_versioned_hashes(mut raw_tx: &[u8]) -> Vec<B256> {
    use alloy_consensus::transaction::RlpEcdsaDecodableTx;
    TxEip4844::rlp_decode_with_signature(&mut raw_tx)
        .map(|(b, _)| b.blob_versioned_hashes)
        .unwrap_or(vec![])
}

fn calculate_versioned_hash(commitment: Bytes48) -> B256 {
    KzgCommitment(*commitment).calculate_versioned_hash()
}

/// Appends the merged blobs to the original blobs bundle.
fn append_merged_blobs(
    original_blobs_bundle: &mut BlobsBundle,
    blobs: &HashMap<B256, BlobWithMetadata>,
    appended_blobs: &Vec<B256>,
    max_blobs_per_block: usize,
) -> Result<(), PayloadMergingError> {
    for vh in appended_blobs {
        let blob_data = blobs.get(vh).ok_or(PayloadMergingError::BlobNotFound)?;
        match blob_data {
            BlobWithMetadata::V1(data) => {
                original_blobs_bundle
                    .push_blob(
                        data.commitment,
                        &[data.proof],
                        data.blob.clone(),
                        max_blobs_per_block,
                    )
                    .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;
            }
            BlobWithMetadata::V2(data) => {
                original_blobs_bundle
                    .push_blob(
                        data.commitment,
                        &data.proofs,
                        data.blob.clone(),
                        max_blobs_per_block,
                    )
                    .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;
            }
        }
    }

    Ok(())
}

fn merged_bid_higher(
    merged_bid: &PayloadHeaderData,
    original_bid: &PayloadHeaderData,
    time: u64,
    max_merged_bid_age_ms: u64,
) -> bool {
    // If the current best bid has equal or higher value, we use that
    if merged_bid.value() <= original_bid.value() {
        debug!(
            "merged bid {:?} with value {:?} is not higher than regular bid, using regular bid, value = {:?}, block_hash = {:?}",
            merged_bid.value(),
            merged_bid.block_hash(),
            original_bid.value(),
            original_bid.block_hash()
        );
        return false;
    }
    // If the merged bid is stale, we use the current best bid
    let now_ms = utcnow_ms();
    if time < now_ms - max_merged_bid_age_ms {
        debug!(
            "merged bid {:?} with value {:?} is stale ({} ms old), using regular bid, value = {:?}, block_hash = {:?}",
            merged_bid.value(),
            merged_bid.block_hash(),
            now_ms - time,
            original_bid.value(),
            original_bid.block_hash()
        );
        return false;
    }

    debug!(
        "using merged bid, value = {:?}, block_hash = {:?}",
        merged_bid.value(),
        merged_bid.block_hash()
    );
    true
}


pub fn record_step(label: &str, duration: Duration) {
    let value = duration.as_nanos() as f64 / 1000.;
    MERGE_TRACE_LATENCY.with_label_values(&[label]).observe(value);
}