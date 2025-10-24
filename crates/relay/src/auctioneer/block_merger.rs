use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    B256, U256,
    map::foldhash::{HashMap, HashMapExt},
};
use helix_common::{chain_info::ChainInfo, local_cache::LocalCache, utils::utcnow_ms};
use helix_types::{
    BlobWithMetadata, BlobsBundle, MergeableOrder, MergeableOrderWithOrigin, MergeableOrders,
    MergedBlock, PayloadAndBlobs,
};
use tracing::{error, warn};

use crate::{
    api::builder::api::get_mergeable_orders,
    auctioneer::{
        BlockMergeResponse, PayloadBidData, PayloadHeaderData, submit_block::ValidatedData,
        types::PayloadEntry,
    },
};

const MERGE_REQUEST_INTERVAL_MS: u64 = 50;

pub struct BlockMerger {
    curr_bid_slot: u64,
    chain_info: ChainInfo,
    local_cache: LocalCache,
    best_merged_block: Option<BestMergedBlock>,
    best_mergeable_orders: BestMergeableOrders,
    base_blocks: HashMap<B256, BaseBlockData>,
    has_new_base_block: bool,
    last_merge_request_time_ms: u64,
}

impl BlockMerger {
    pub fn new(curr_bid_slot: u64, chain_info: ChainInfo, local_cache: LocalCache) -> Self {
        Self {
            curr_bid_slot,
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
        self.best_mergeable_orders.reset(bid_slot, self.chain_info.max_blobs_per_block());
        self.base_blocks.clear();
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = 0;
    }

    pub fn get_header(&self, parent_hash: &B256) -> Option<(u64, PayloadHeaderData)> {
        self.best_merged_block.as_ref().and_then(|entry| {
            if &entry.bid.payload_and_blobs.execution_payload.parent_hash == parent_hash {
                Some((entry.base_block_time_ms, entry.bid.clone()))
            } else {
                None
            }
        })
    }

    pub fn update(&mut self, block: &ValidatedData<'_>) {
        if let Some(merging_data) = &block.merging_data {
            let mergeable_orders =
                get_mergeable_orders(&block.submission, merging_data.clone()).ok();

            if let Some(orders) = mergeable_orders {
                let max_blobs_per_block = self.chain_info.max_blobs_per_block();
                self.best_mergeable_orders.insert_orders(
                    self.curr_bid_slot,
                    max_blobs_per_block,
                    block.submission.value(),
                    orders,
                );
            }

            if block.is_top_bid && merging_data.allow_appending {
                self.update_base_block(block);
            }
        }
    }

    pub fn should_request_merge(&self) -> bool {
        let has_new_data = self.best_mergeable_orders.has_new_orders() ||
            (self.best_mergeable_orders.has_orders() && self.has_new_base_block);
        let time_since_last_request_ms =
            utcnow_ms().saturating_sub(self.last_merge_request_time_ms);
        has_new_data && time_since_last_request_ms >= MERGE_REQUEST_INTERVAL_MS
    }

    pub fn fetch_best_mergeable_orders(
        &mut self,
    ) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        self.has_new_base_block = false;
        self.last_merge_request_time_ms = utcnow_ms();
        self.best_mergeable_orders.load()
    }

    pub fn prepare_merged_payload_for_storage(
        &mut self,
        response: BlockMergeResponse,
        original_payload: Arc<PayloadAndBlobs>,
    ) -> Result<(B256, PayloadEntry), PayloadMergingError> {
        let bid_slot = self.curr_bid_slot;
        let max_blobs_per_block = self.best_mergeable_orders.max_blobs_per_block;

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
            original_tx_count: base_block_data.num_transactions,
            merged_tx_count: response.execution_payload.transactions.len(),
            original_blob_count: base_block_data.num_blobs,
            merged_blob_count: base_block_data.num_blobs + response.appended_blobs.len(),
            builder_inclusions: response.builder_inclusions.clone(),
        });

        let blobs = &self.best_mergeable_orders.mergeable_blob_bundles;

        let mut merged_blobs_bundle = Arc::unwrap_or_clone(original_payload).blobs_bundle;
        append_merged_blobs(&mut merged_blobs_bundle, blobs, &response, max_blobs_per_block)?;

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

        // Return the payload entry to be stored for get payload calls
        Ok((block_hash, PayloadEntry { payload_and_blobs, bid_data: Some(bid_data) }))
    }

    fn update_base_block(&mut self, base_block: &ValidatedData<'_>) {
        let block_hash = *base_block.submission.block_hash();
        if self.base_blocks.contains_key(&block_hash) {
            return;
        }

        let base_block_time_ms = utcnow_ms();

        let base_block_data = BaseBlockData {
            time_ms: base_block_time_ms,
            value: base_block.submission.value(),
            num_transactions: base_block.submission.num_txs(),
            num_blobs: base_block.submission.blobs_bundle().blobs().len(),
        };

        self.base_blocks.insert(block_hash, base_block_data);
        self.has_new_base_block = true;
    }
}

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

/// Appends the merged blobs to the original blobs bundle.
fn append_merged_blobs(
    original_blobs_bundle: &mut BlobsBundle,
    blobs: &HashMap<B256, BlobWithMetadata>,
    response: &BlockMergeResponse,
    max_blobs_per_block: usize,
) -> Result<(), PayloadMergingError> {
    for vh in &response.appended_blobs {
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

struct BaseBlockData {
    time_ms: u64,
    value: U256,
    num_transactions: usize,
    num_blobs: usize,
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
pub struct BestMergeableOrders {
    current_slot: u64,
    max_blobs_per_block: usize,
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
    pub fn new() -> Self {
        Self {
            current_slot: 0,
            max_blobs_per_block: 0,
            order_map: HashMap::with_capacity(5000),
            mergeable_blob_bundles: HashMap::with_capacity(5000),
            best_orders: Vec::with_capacity(5000),
            has_new_orders: false,
        }
    }

    pub fn load(
        &mut self,
    ) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        // Reset the new orders flag
        self.has_new_orders = false;
        Some((&self.best_orders, &self.mergeable_blob_bundles))
    }

    pub fn has_new_orders(&self) -> bool {
        self.has_new_orders && !self.best_orders.is_empty()
    }

    pub fn has_orders(&self) -> bool {
        !self.best_orders.is_empty()
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    pub fn insert_orders(
        &mut self,
        slot: u64,
        max_blobs_per_block: usize,
        bid_value: U256,
        mergeable_orders: MergeableOrders,
    ) {
        // If the orders are for a newer slot, reset current state.
        if self.current_slot < slot {
            self.reset(slot, max_blobs_per_block);
        }
        let origin = mergeable_orders.origin;
        // Insert each order into the order map
        mergeable_orders.orders.into_iter().for_each(|o| {
            // Collect all blobs for this order
            match self.order_map.entry(o) {
                // If the order already exists, keep the one with the highest bid
                Entry::Occupied(mut e) if e.get().value < bid_value => {
                    // We update the value of the bid to the highest
                    e.get_mut().value = bid_value;
                    // and the origin for the order
                    self.best_orders[e.get().index].origin = origin;
                }
                Entry::Occupied(_) => {}
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
    }

    pub fn reset(&mut self, slot: u64, max_blobs_per_block: usize) {
        self.current_slot = slot;
        self.max_blobs_per_block = max_blobs_per_block;
        self.order_map.clear();
        self.best_orders.clear();
        self.mergeable_blob_bundles.clear();
    }
}
