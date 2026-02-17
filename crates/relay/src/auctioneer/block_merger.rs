use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Bytes48, Transaction, TxEnvelope, TxType};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rlp::Decodable;
use helix_common::{
    RelayConfig,
    api::builder_api::InclusionListWithMetadata,
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::MERGE_TRACE_LATENCY,
    utils::{utcnow_ms, utcnow_ns},
};
use helix_types::{
    BlobWithMetadata, BlobWithMetadataV1, BlobWithMetadataV2, BlobsBundle, BlobsBundleVersion,
    BlockMergingData, BlsPublicKeyBytes, BundleOrder, KzgCommitment, MergeableBundle,
    MergeableOrder, MergeableOrderWithOrigin, MergeableOrders, MergeableTransaction, MergedBlock,
    MergedBlockTrace, Order, PayloadAndBlobs, SignedBidSubmission, Transactions,
};
use rustc_hash::{FxBuildHasher, FxHashSet};
use serde_json::json;
use tracing::{debug, error, info, trace, warn};
use zstd::zstd_safe::WriteBuf;

use crate::auctioneer::{
    BlockMergeRequest, BlockMergeRequestRef, BlockMergeResponse, PayloadBidData,
    submit_block::MergeData, types::PayloadEntry,
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
    #[error("transaction decode failure")]
    FailedToDecode(#[from] alloy_rlp::Error),
    #[error("zero value transaction")]
    ZeroValue,
}

pub struct BlockMerger {
    curr_bid_slot: u64,
    config: RelayConfig,
    chain_info: ChainInfo,
    local_cache: LocalCache,
    best_merged_block: Option<BestMergedBlock>,
    best_mergeable_orders: BestMergeableOrders,
    appendable_blocks: HashMap<BlockHash, AppendableBlockData>,
    base_block: Option<BlockHash>,
    has_new_base_block: bool,
    last_merge_request_time_ms: u64,
    base_txs_set: FxHashSet<Bytes>,
    trimmed_orders_buf: Vec<MergeableOrderWithOrigin>,
    inserted_orders_count: usize,
    inserted_appendable_blocks_count: usize,
    updated_base_block_count: usize,
    fetch_merge_request_count: usize,
    proceeding_merge_request_count: usize,
    no_base_block_count: usize,
    no_appendable_block_data_count: usize,
    no_additional_orders_count: usize,
    found_orders_count: usize,
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
            appendable_blocks: HashMap::with_capacity(00),
            base_block: None,
            has_new_base_block: false,
            last_merge_request_time_ms: 0,
            base_txs_set: FxHashSet::with_capacity_and_hasher(400, FxBuildHasher),
            trimmed_orders_buf: Vec::with_capacity(400),
            inserted_orders_count: 0,
            inserted_appendable_blocks_count: 0,
            updated_base_block_count: 0,
            fetch_merge_request_count: 0,
            proceeding_merge_request_count: 0,
            no_base_block_count: 0,
            no_appendable_block_data_count: 0,
            no_additional_orders_count: 0,
            found_orders_count: 0,
        }
    }
}

impl BlockMerger {
    pub fn on_new_slot(&mut self, bid_slot: u64) {
        info!(old_slot = %self.curr_bid_slot, new_slot = %bid_slot, inserted_appendable_blocks_count = %self.inserted_appendable_blocks_count, inserted_orders_count = %self.inserted_orders_count, updated_base_block_count = %self.updated_base_block_count, fetch_merge_request_count = %self.fetch_merge_request_count, proceeding_merge_request_count = %self.proceeding_merge_request_count, no_base_block_count = %self.no_base_block_count, no_appendable_block_data_count = %self.no_appendable_block_data_count, found_orders_count = %self.found_orders_count, "resetting block merger slot");
        self.curr_bid_slot = bid_slot;
        self.best_merged_block = None;
        self.best_mergeable_orders.reset();
        self.appendable_blocks.clear();
        self.base_block = None;
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = 0;
        self.base_txs_set.clear();
        self.trimmed_orders_buf.clear();
        self.found_orders_count = 0;
        self.inserted_orders_count = 0;
        self.inserted_appendable_blocks_count = 0;
        self.updated_base_block_count = 0;
        self.fetch_merge_request_count = 0;
        self.proceeding_merge_request_count = 0;
        self.no_base_block_count = 0;
        self.no_appendable_block_data_count = 0;
        self.no_additional_orders_count = 0;
        self.found_orders_count = 0;
    }

    pub fn add_inclusion_list(&mut self, inclusion_list: &InclusionListWithMetadata) {
        if let Some(il_cfg) = &self.config.inclusion_list {
            debug!("adding {} inclusion list orders to merging pool", inclusion_list.txs.len());

            inclusion_list.txs.iter().for_each(|o| {
                self.best_mergeable_orders.order_map.entry(o.bytes.clone().into()).or_insert_with(
                    || {
                        self.best_mergeable_orders.has_new_orders = true;
                        let index = self.best_mergeable_orders.best_orders.len();
                        self.best_mergeable_orders.best_orders.push(MergeableOrderWithOrigin::new(
                            il_cfg.relay_address,
                            o.bytes.clone().into(),
                        ));
                        OrderMetadata { value: U256::ZERO, index }
                    },
                );
            });
        }
    }

    pub fn get_header(&self, original_bid: &PayloadEntry) -> Option<PayloadEntry> {
        trace!("fetching merged header");
        let start_time = Instant::now();
        let entry = self.best_merged_block.as_ref()?;
        if !merged_bid_higher(
            &entry.bid,
            original_bid,
            entry.base_block_time_ms,
            self.config.block_merging_config.max_merged_bid_age_ms,
        ) {
            trace!("merged bid not higher");
            return None;
        }

        if entry.bid.parent_hash() != original_bid.parent_hash() {
            trace!("merged bid parent hash does not match original bid parent hash");
            return None;
        }

        record_step("get_header", start_time.elapsed());
        trace!("fetched merged header");

        if self.config.block_merging_config.is_dry_run {
            trace!("dry run mode enabled, not returning merged header");
            return None;
        }
        Some(entry.bid.clone())
    }

    pub fn update_base_block(&mut self, base_block: BlockHash) {
        if let Some(base_block_data) = self.appendable_blocks.get(&base_block) {
            trace!(%base_block,"updating base block");
            self.updated_base_block_count += 1;
            self.base_block = Some(base_block);
            self.has_new_base_block = true;
            self.base_txs_set.clear();
            self.base_txs_set.extend(
                base_block_data.execution_payload.transactions.iter().map(|tx| tx.0.clone()),
            );
        }
    }

    pub fn insert_merge_data(&mut self, merging_data: MergeData) {
        if !merging_data.merging_data.orders.orders.is_empty() {
            trace!(%merging_data.block_hash,"inserting merge orders from block");
            self.inserted_orders_count += 1;
            self.best_mergeable_orders
                .insert_orders(merging_data.block_value, merging_data.merging_data.orders);
        }

        if merging_data.merging_data.allow_appending {
            trace!(%merging_data.block_hash,"inserting new appendable block data");
            self.inserted_appendable_blocks_count += 1;
            self.insert_appendable_block_data(
                merging_data.slot,
                &merging_data.block_hash,
                merging_data.block_value,
                merging_data.proposer_fee_recipient,
                merging_data.parent_beacon_block_root,
                merging_data.execution_payload,
            );
        }
    }

    pub fn fetch_merge_request(&mut self) -> Option<BlockMergeRequest> {
        trace!("fetching merge request");
        self.fetch_merge_request_count += 1;
        if !self.should_request_merge() {
            trace!("should not request merge");
            return None;
        }

        trace!("proceeding with merge request");
        self.proceeding_merge_request_count += 1;

        let start_time = Instant::now();
        let Some(base_block_hash) = self.base_block else {
            error!("no base block set for merge request");
            self.no_base_block_count += 1;
            return None;
        };

        let Some(base_block) = self.appendable_blocks.get(&base_block_hash) else {
            error!(%base_block_hash, "could not find base block data for merge request");
            self.no_appendable_block_data_count += 1;
            return None;
        };

        self.best_mergeable_orders.has_new_orders = false;

        self.trimmed_orders_buf.clear();
        self.trimmed_orders_buf.extend(
            self.best_mergeable_orders
                .best_orders
                .iter()
                .filter(|o| match &o.order {
                    MergeableOrder::Tx(tx) => {
                        if is_blob_transaction(&tx.transaction) {
                            trace!(tx = ?tx.transaction, "blob transaction in best mergeable orders tx");
                        }
                        !self.base_txs_set.contains(tx.transaction.as_slice())
                    }
                    MergeableOrder::Bundle(b) => {
                        !b.transactions.iter().any(|t| {
                            if is_blob_transaction(t) {
                                trace!(tx = ?t, "blob transaction in best mergeable orders bundle");
                            }
                            self.base_txs_set.contains(t.as_slice())
                        })
                    }
                })
                .cloned(),
        );

        if self.trimmed_orders_buf.is_empty() {
            trace!("no additional orders to merge");
            self.no_additional_orders_count += 1;
            return None;
        }

        trace!(count = self.trimmed_orders_buf.len(), "found orders");
        self.found_orders_count += 1;

        let merge_request = BlockMergeRequest {
            bid_slot: base_block.slot,
            request: json!(BlockMergeRequestRef {
                original_value: base_block.value,
                proposer_fee_recipient: base_block.proposer_fee_recipient,
                execution_payload: &base_block.execution_payload,
                parent_beacon_block_root: base_block.parent_beacon_block_root,
                merging_data: &self.trimmed_orders_buf,
                trace: MergedBlockTrace {
                    request_time_ns: utcnow_ns(),
                    sim_start_time_ns: 0,
                    sim_end_time_ns: 0,
                    finalize_time_ns: 0,
                },
            }),
            block_hash: base_block_hash,
        };

        self.has_new_base_block = false;
        self.last_merge_request_time_ms = utcnow_ms();
        record_step("fetch_merge_request", start_time.elapsed());
        trace!("fetched merge request");
        Some(merge_request)
    }

    pub fn prepare_merged_payload_for_storage(
        &mut self,
        response: BlockMergeResponse,
        original_payload: PayloadAndBlobs,
        builder_pubkey: BlsPublicKeyBytes,
    ) -> Result<PayloadEntry, PayloadMergingError> {
        debug!(?response.builder_inclusions, %response.proposer_value, "preparing merged payload for storage");
        let start_time = Instant::now();
        let bid_slot = self.curr_bid_slot;
        let max_blobs_per_block = self.chain_info.max_blobs_per_block();

        let base_block_data = match self.appendable_blocks.get(&response.base_block_hash) {
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

        let mut trace = response.trace;
        trace.finalize_time_ns = utcnow_ns();

        self.local_cache.save_merged_block(MergedBlock {
            slot: bid_slot,
            block_number: response.execution_payload.block_number,
            original_block_hash: response.base_block_hash,
            block_hash,
            original_value: base_block_data.value,
            merged_value: response.proposer_value,
            original_tx_count: original_payload.execution_payload.transactions.len(),
            merged_tx_count: response.execution_payload.transactions.len(),
            original_blob_count: original_payload.blobs_bundle.blobs().len(),
            merged_blob_count: original_payload.blobs_bundle.blobs().len() +
                response.appended_blobs.len(),
            builder_inclusions: response.builder_inclusions,
            trace,
        });

        trace!(%block_hash, "stored merged block in local cache");

        let blobs = &self.best_mergeable_orders.mergeable_blob_bundles;

        let mut merged_blobs_bundle = original_payload.blobs_bundle.as_ref().to_owned();
        append_merged_blobs(
            &mut merged_blobs_bundle,
            blobs,
            &response.appended_blobs,
            max_blobs_per_block,
        )?;

        let withdrawals_root = response.execution_payload.withdrawals_root();

        let payload_and_blobs = PayloadAndBlobs {
            execution_payload: Arc::new(response.execution_payload),
            blobs_bundle: Arc::new(merged_blobs_bundle),
        };

        let bid_data = PayloadBidData {
            withdrawals_root,
            execution_requests: Arc::new(response.execution_requests),
            value: response.proposer_value,
            tx_root: None,
            builder_pubkey,
        };

        trace!(%block_hash, %response.proposer_value, "blobs appended to merged payload");

        let new_bid = PayloadEntry::new_gossip(payload_and_blobs, bid_data);

        // Store locally to serve header requests
        self.best_merged_block = Some(BestMergedBlock {
            base_block_time_ms: base_block_data.time_ms,
            bid: new_bid.clone(),
        });

        record_step("prepare_merged_payload_for_storage", start_time.elapsed());

        // Return the payload entry to be stored for get payload calls
        Ok(new_bid)
    }

    fn should_request_merge(&self) -> bool {
        let start_time = Instant::now();
        let has_new_data = self.best_mergeable_orders.has_new_orders() ||
            (self.best_mergeable_orders.has_orders() && self.has_new_base_block);
        if !has_new_data {
            return false;
        }
        let res = utcnow_ms().saturating_sub(self.last_merge_request_time_ms) >=
            MERGE_REQUEST_INTERVAL_MS;
        record_step("should_request_merge", start_time.elapsed());
        res
    }

    fn insert_appendable_block_data(
        &mut self,
        slot: u64,
        base_block_hash: &B256,
        base_block_value: U256,
        proposer_fee_recipient: Address,
        parent_beacon_block_root: Option<B256>,
        execution_payload: helix_types::ExecutionPayload,
    ) {
        if self.appendable_blocks.contains_key(base_block_hash) {
            return;
        }

        let start_time = Instant::now();

        let base_block_time_ms = utcnow_ms();

        let appendable_block_data = AppendableBlockData {
            slot,
            time_ms: base_block_time_ms,
            value: base_block_value,
            proposer_fee_recipient,
            parent_beacon_block_root,
            execution_payload,
        };

        self.appendable_blocks.insert(*base_block_hash, appendable_block_data);
        record_step("insert_appendable_block", start_time.elapsed());
    }
}

struct AppendableBlockData {
    slot: u64,
    time_ms: u64,
    value: U256,
    proposer_fee_recipient: Address,
    parent_beacon_block_root: Option<B256>,
    execution_payload: helix_types::ExecutionPayload,
}

struct BestMergedBlock {
    base_block_time_ms: u64,
    bid: PayloadEntry,
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
        .filter_map(|order| match order_to_mergeable(order, txs, &blob_versioned_hashes) {
            Err(err) => {
                debug!(?order, ?err, "dropping invalid order during mergeable orders extraction");
                None
            }
            other => Some(other),
        })
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
                        // If the tx references bundles not in the block, we drop the bundle
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
    let mut missing_blobs =
        versioned_hashes.iter().map(|h| !blob_versioned_hashes.iter().any(|vh| vh == h));
    if missing_blobs.any(|f| f) {
        debug!(%num_blobs, num_blob_versioned_hashes=%blob_versioned_hashes.len(), raw_tx=?raw_tx, "blob tx refs blobs not in the block");
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

fn is_blob_transaction(raw_tx: &Bytes) -> bool {
    // First byte is always the transaction type, or >= 0xc0 for legacy
    // (source: https://eips.ethereum.org/EIPS/eip-2718)
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
        TxEnvelope::Eip4844(tx_eip4844) => match tx_eip4844.blob_versioned_hashes() {
            Some(vhs) => vhs.to_vec(),
            None => vec![],
        },
        _ => vec![],
    }
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
    merged_bid: &PayloadEntry,
    original_bid: &PayloadEntry,
    time: u64,
    max_merged_bid_age_ms: u64,
) -> bool {
    // If the current best bid has equal or higher value, we use that
    if merged_bid.value() <= original_bid.value() {
        debug!(
            "merged bid {:?} with value {:?} is not higher than regular bid, using regular bid, value = {:?}, block_hash = {:?}",
            merged_bid.block_hash(),
            merged_bid.value(),
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
    let value = duration.as_nanos() as f64 / 1000.0;
    MERGE_TRACE_LATENCY.with_label_values(&[label]).observe(value);
}
