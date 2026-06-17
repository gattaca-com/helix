use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, Bytes, U256};
use helix_common::{
    RelayConfig,
    chain_info::ChainInfo,
    metrics::MERGE_TRACE_LATENCY,
    utils::{utcnow_ms, utcnow_ns},
};
use helix_types::{
    BlobWithMetadata, BlobsBundle, BlsPublicKeyBytes, ExecutionPayload, MergeableOrder,
    MergeableOrderWithOrigin, MergeableOrders, MergedBlockTrace, PayloadAndBlobs,
};
use rustc_hash::{FxBuildHasher, FxHashSet};
use tracing::{error, trace, warn};

use crate::{
    PayloadEntry,
    auctioneer::{BlockMergeRequestRef, MergeRequest, PayloadBidData},
    simulator::BlockMergeResponse,
};

const MERGE_REQUEST_INTERVAL_MS: u64 = 50;

type BlockHash = B256;

#[derive(Debug, thiserror::Error)]
pub enum PayloadAssemblyError {
    #[error("base block not found in appendable_blocks")]
    BaseBlockNotFound,
    #[error("merged value {merged} not higher than base value {original}")]
    NotValuable { original: U256, merged: U256 },
    #[error("blob not found in order pool")]
    BlobNotFound,
    #[error("max blob count exceeded")]
    MaxBlobCount,
}

#[derive(Debug, Clone)]
pub struct ValidatedMergeCandidate {
    pub block_hash: B256,
    pub slot: u64,
    pub value: U256,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: Option<B256>,
    pub allow_appending: bool,
    pub is_top_bid: bool,
    pub mergeable_orders: MergeableOrders,
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: Arc<BlobsBundle>,
    pub builder_pubkey: BlsPublicKeyBytes,
}

pub struct SimBlockMerger {
    curr_bid_slot: u64,
    relay_config: RelayConfig,
    chain_info: ChainInfo,
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
    no_additional_orders_count: usize,
    found_orders_count: usize,
}

impl SimBlockMerger {
    pub fn new(chain_info: ChainInfo, relay_config: RelayConfig) -> Self {
        Self {
            curr_bid_slot: 0,
            relay_config,
            chain_info,
            best_mergeable_orders: BestMergeableOrders::new(),
            appendable_blocks: HashMap::with_capacity(200),
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
            no_additional_orders_count: 0,
            found_orders_count: 0,
        }
    }

    pub fn on_new_slot(&mut self, bid_slot: u64) {
        self.curr_bid_slot = bid_slot;
        self.best_mergeable_orders.reset();
        self.appendable_blocks.clear();
        self.base_block = None;
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = 0;
        self.base_txs_set.clear();
        self.trimmed_orders_buf.clear();
        self.inserted_orders_count = 0;
        self.inserted_appendable_blocks_count = 0;
        self.updated_base_block_count = 0;
        self.fetch_merge_request_count = 0;
        self.proceeding_merge_request_count = 0;
        self.no_base_block_count = 0;
        self.no_additional_orders_count = 0;
        self.found_orders_count = 0;
    }

    pub fn add_inclusion_list(
        &mut self,
        inclusion_list: &helix_common::api::builder_api::InclusionListWithMetadata,
    ) {
        if let Some(il_cfg) = &self.relay_config.inclusion_list {
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

    /// Called after a successful validation: feed orders and appendable block data
    /// extracted from the submission directly in the simulator tile.
    pub fn ingest_validated_candidate(&mut self, candidate: ValidatedMergeCandidate) {
        if !candidate.mergeable_orders.orders.is_empty() {
            trace!(%candidate.block_hash, "sim_block_merger: inserting orders");
            self.inserted_orders_count += 1;
            self.best_mergeable_orders
                .insert_orders(candidate.value, candidate.mergeable_orders.clone());
        }

        if candidate.allow_appending {
            trace!(%candidate.block_hash, "sim_block_merger: inserting appendable block");
            self.inserted_appendable_blocks_count += 1;
            if !self.appendable_blocks.contains_key(&candidate.block_hash) {
                self.appendable_blocks.insert(candidate.block_hash, AppendableBlockData {
                    slot: candidate.slot,
                    value: candidate.value,
                    proposer_fee_recipient: candidate.proposer_fee_recipient,
                    parent_beacon_block_root: candidate.parent_beacon_block_root,
                    execution_payload: candidate.execution_payload,
                    blobs_bundle: candidate.blobs_bundle,
                    builder_pubkey: candidate.builder_pubkey,
                });
            }
        }

        if candidate.is_top_bid && candidate.allow_appending {
            self.update_base_block(candidate.block_hash);
        }
    }

    /// Called when a new top-bid arrives: update base block tracking.
    pub fn update_base_block(&mut self, base_block: BlockHash) {
        if let Some(base_block_data) = self.appendable_blocks.get(&base_block) {
            trace!(%base_block, "sim block_merger: updating base block");
            self.updated_base_block_count += 1;
            self.base_block = Some(base_block);
            self.has_new_base_block = true;
            self.base_txs_set.clear();
            self.base_txs_set.extend(
                base_block_data.execution_payload.transactions.iter().map(|tx| tx.0.clone()),
            );
        }
    }

    /// Attempt to produce a merge request. Returns `None` if conditions are not met.
    pub fn fetch_merge_request(&mut self) -> Option<MergeRequest> {
        let start_time = Instant::now();
        self.fetch_merge_request_count += 1;
        if !self.should_request_merge() {
            return None;
        }
        self.proceeding_merge_request_count += 1;

        let base_block_hash = match self.base_block {
            Some(h) => h,
            None => {
                error!("sim_block_merger: no base block set");
                self.no_base_block_count += 1;
                return None;
            }
        };
        let base_block = match self.appendable_blocks.get(&base_block_hash) {
            Some(b) => b,
            None => {
                error!(%base_block_hash, "sim_block_merger: base block data missing");
                return None;
            }
        };

        self.best_mergeable_orders.has_new_orders = false;
        self.trimmed_orders_buf.clear();
        self.trimmed_orders_buf.extend(
            self.best_mergeable_orders
                .best_orders
                .iter()
                .filter(|o| match &o.order {
                    MergeableOrder::Tx(tx) => !self.base_txs_set.contains(tx.transaction.as_ref()),
                    MergeableOrder::Bundle(b) => {
                        !b.transactions.iter().any(|t| self.base_txs_set.contains(t.as_ref()))
                    }
                })
                .cloned(),
        );

        if self.trimmed_orders_buf.is_empty() {
            self.no_additional_orders_count += 1;
            return None;
        }
        self.found_orders_count += 1;

        let merge_request = MergeRequest {
            bid_slot: base_block.slot,
            request: serde_json::json!(BlockMergeRequestRef {
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
        record_step("fetch_merge_request", start_time.elapsed()); // timing happens at call site
        Some(merge_request)
    }

    /// Assemble the merged `PayloadEntry` on the sim-tile thread after
    /// `do_merge_request` succeeds. All `BlockMerger` state is on this thread
    /// so no locking is needed.
    ///
    /// On success returns the `PayloadEntry` that the auctioneer should store
    /// in `ctx.payloads`. It is sent via `SimResult::MergedPayload`.
    pub fn assemble_payload(
        &mut self,
        response: BlockMergeResponse,
        builder_pubkey: BlsPublicKeyBytes,
        original_blobs_bundle: Arc<BlobsBundle>,
    ) -> Result<PayloadEntry, PayloadAssemblyError> {
        let max_blobs = self.chain_info.max_blobs_per_block();

        let base_data = self
            .appendable_blocks
            .get(&response.base_block_hash)
            .ok_or(PayloadAssemblyError::BaseBlockNotFound)?;

        if response.proposer_value <= base_data.value {
            warn!(
                original = %base_data.value,
                merged = %response.proposer_value,
                "merged payload not more valuable than base"
            );
            return Err(PayloadAssemblyError::NotValuable {
                original: base_data.value,
                merged: response.proposer_value,
            });
        }

        let blob_pool = &self.best_mergeable_orders.mergeable_blob_bundles;
        let mut merged_blobs = original_blobs_bundle.as_ref().to_owned();
        for vh in &response.appended_blobs {
            let blob_data = blob_pool.get(vh).ok_or(PayloadAssemblyError::BlobNotFound)?;
            merged_blobs
                .push_blob(
                    blob_data.commitment,
                    &blob_data.proofs,
                    blob_data.blob.clone(),
                    max_blobs,
                )
                .map_err(|_| PayloadAssemblyError::MaxBlobCount)?;
        }

        let withdrawals_root = response.execution_payload.withdrawals_root();
        let payload_and_blobs = PayloadAndBlobs {
            execution_payload: Arc::new(response.execution_payload),
            blobs_bundle: Arc::new(merged_blobs),
        };
        let bid_data = PayloadBidData {
            withdrawals_root,
            execution_requests: Arc::new(response.execution_requests),
            value: response.proposer_value,
            tx_root: None,
            builder_pubkey,
        };

        Ok(PayloadEntry::new_gossip(payload_and_blobs, bid_data))
    }

    fn should_request_merge(&self) -> bool {
        if self.base_block.is_none() {
            return false;
        }
        let has_new = self.best_mergeable_orders.has_new_orders() ||
            (self.best_mergeable_orders.has_orders() && self.has_new_base_block);
        if !has_new {
            return false;
        }
        utcnow_ms().saturating_sub(self.last_merge_request_time_ms) >= MERGE_REQUEST_INTERVAL_MS
    }

    pub fn take_base_info(
        &self,
        hash: B256,
    ) -> Option<(BlsPublicKeyBytes, Arc<BlobsBundle>, usize)> {
        self.appendable_blocks.get(&hash).map(|d| {
            (d.builder_pubkey, d.blobs_bundle.clone(), d.execution_payload.transactions.len())
        })
    }
}

struct AppendableBlockData {
    slot: u64,
    value: U256,
    proposer_fee_recipient: Address,
    parent_beacon_block_root: Option<B256>,
    execution_payload: helix_types::ExecutionPayload,
    blobs_bundle: Arc<BlobsBundle>,
    builder_pubkey: BlsPublicKeyBytes,
}

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    index: usize,
}

struct BestMergeableOrders {
    order_map: HashMap<MergeableOrder, OrderMetadata>,
    best_orders: Vec<MergeableOrderWithOrigin>,
    mergeable_blob_bundles: HashMap<B256, BlobWithMetadata>,
    has_new_orders: bool,
}

impl BestMergeableOrders {
    fn new() -> Self {
        Self {
            order_map: HashMap::with_capacity(5000),
            best_orders: Vec::with_capacity(5000),
            mergeable_blob_bundles: HashMap::with_capacity(5000),
            has_new_orders: false,
        }
    }

    fn has_new_orders(&self) -> bool {
        self.has_new_orders && !self.best_orders.is_empty()
    }

    fn has_orders(&self) -> bool {
        !self.best_orders.is_empty()
    }

    fn insert_orders(&mut self, bid_value: U256, orders: MergeableOrders) {
        let origin = orders.origin;
        for o in orders.orders {
            match self.order_map.entry(o) {
                Entry::Occupied(mut e) => {
                    if e.get().value < bid_value {
                        e.get_mut().value = bid_value;
                        self.best_orders[e.get().index].origin = origin;
                    }
                }
                Entry::Vacant(e) => {
                    self.has_new_orders = true;
                    let index = self.best_orders.len();
                    self.best_orders.push(MergeableOrderWithOrigin::new(origin, e.key().clone()));
                    e.insert(OrderMetadata { value: bid_value, index });
                }
            }
        }
        self.mergeable_blob_bundles.extend(orders.blobs);
    }

    fn reset(&mut self) {
        self.order_map.clear();
        self.best_orders.clear();
        self.mergeable_blob_bundles.clear();
        self.has_new_orders = false;
    }
}

pub fn record_step(label: &str, duration: Duration) {
    let value = duration.as_nanos() as f64 / 1000.0;
    MERGE_TRACE_LATENCY.with_label_values(&[label]).observe(value);
}
