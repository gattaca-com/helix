use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256};
use flux_profiler::timed;
use helix_common::{
    RelayConfig,
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::MERGE_TRACE_LATENCY,
    utils::{utcnow_ms, utcnow_ns},
};
use helix_types::{
    BlobWithMetadata, BlobsBundle, BlsPublicKeyBytes, MergedBlock, PayloadAndBlobs, Transactions,
};
use rustc_hash::{FxBuildHasher, FxHashSet};
use tracing::{debug, info, trace, warn};

use crate::auctioneer::{BlockMergeResponse, PayloadBidData, types::PayloadEntry};

type BlockHash = B256;

#[derive(Debug, thiserror::Error)]
pub enum PayloadMergingError {
    #[error(
        "merged payload value is lower or equal to original bid. original: {original}, merged: {merged}"
    )]
    MergedPayloadNotValuable { original: U256, merged: U256 },
    #[error("reached maximum blob count for block")]
    MaxBlobCountReached,
}

/// Stores merged blocks so they can be served via `get_header`/`get_payload`. Everything
/// needed to *build* a merged block (mergeable orders, blob sidecars, base-block tracking)
/// now lives in `BlockMergingTile`, which forwards submissions to the merge builder directly
/// and re-attaches blob sidecars from its own cache before handing a `BlockMergeResponse`
/// back here.
pub struct BlockMerger {
    curr_bid_slot: u64,
    config: RelayConfig,
    chain_info: ChainInfo,
    local_cache: LocalCache,
    best_merged_blocks: HashMap<Address, BestMergedBlock>,
    /// Base block hashes for which `get_header` found that the merged bid only differed
    /// from the original in the payment tx (and lost out on value because of it). Checked
    /// again in `prepare_merged_payload_for_storage` so we can log when the same original
    /// block gets reprocessed.
    flagged_payment_tx_only_blocks: FxHashSet<BlockHash>,
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
            best_merged_blocks: HashMap::with_capacity(16),
            flagged_payment_tx_only_blocks: FxHashSet::with_capacity_and_hasher(16, FxBuildHasher),
        }
    }
}

impl BlockMerger {
    pub fn on_new_slot(&mut self, bid_slot: u64) {
        info!(old_slot = %self.curr_bid_slot, new_slot = %bid_slot, "resetting block merger slot");
        self.curr_bid_slot = bid_slot;
        self.best_merged_blocks.clear();
        self.flagged_payment_tx_only_blocks.clear();
    }

    #[timed]
    pub fn get_header(
        &mut self,
        original_bid: &PayloadEntry,
        is_mev_boost: bool,
    ) -> Option<PayloadEntry> {
        trace!("fetching merged header");
        let start_time = Instant::now();
        let coinbase = original_bid.execution_payload().fee_recipient;
        let entry = self.best_merged_blocks.get(&coinbase)?;

        if !merged_bid_higher(
            &entry.bid,
            original_bid,
            entry.base_block_time_ms,
            self.config.block_merging_config.max_merged_bid_age_ms,
        ) {
            trace!("merged bid not higher");
            let original_block_hash = *original_bid.block_hash();
            if log_if_only_payment_tx_changed(
                original_block_hash,
                *entry.bid.block_hash(),
                entry.bid.bid_data_ref().builder_pubkey,
                &original_bid.execution_payload().transactions,
                &entry.bid.execution_payload().transactions,
            ) {
                self.flagged_payment_tx_only_blocks.insert(original_block_hash);
            }
            return None;
        }

        if entry.bid.parent_hash() != original_bid.parent_hash() {
            trace!("merged bid parent hash does not match original bid parent hash");
            return None;
        }

        record_step("get_header", start_time.elapsed());
        trace!("fetched merged header");

        if is_mev_boost {
            self.local_cache.set_merged_block_header_served(entry.bid.block_hash(), utcnow_ns());
        }

        if self.config.block_merging_config.is_dry_run {
            info!("dry run mode enabled, not returning merged header");
            return None;
        }
        Some(entry.bid.clone())
    }

    #[timed]
    pub fn prepare_merged_payload_for_storage(
        &mut self,
        response: BlockMergeResponse,
        original_payload: PayloadAndBlobs,
        original_value: U256,
        builder_pubkey: BlsPublicKeyBytes,
    ) -> Result<PayloadEntry, PayloadMergingError> {
        debug!(?response.builder_inclusions, %response.proposer_value, "preparing merged payload for storage");
        let start_time = Instant::now();

        if response.proposer_value <= original_value {
            warn!(
                original = %original_value,
                merged = %response.proposer_value,
                "merged payload value is not higher than original bid"
            );
            return Err(PayloadMergingError::MergedPayloadNotValuable {
                original: original_value,
                merged: response.proposer_value,
            });
        }

        let bid_slot = self.curr_bid_slot;
        let max_blobs_per_block = self.chain_info.max_blobs_per_block();

        let original_block_hash = original_payload.execution_payload.block_hash;
        if self.flagged_payment_tx_only_blocks.remove(&original_block_hash) {
            info!(
                %original_block_hash,
                %builder_pubkey,
                "original payload previously flagged for a payment-tx-only merge is being merged again"
            );
        }

        let block_hash = response.execution_payload.block_hash;
        let base_block_time_ms = response.trace.request_time_ns / 1_000_000;

        let mut trace = response.trace;
        trace.finalize_time_ns = utcnow_ns();

        self.local_cache.save_merged_block(MergedBlock {
            slot: bid_slot,
            block_number: response.execution_payload.block_number,
            original_block_hash: response.base_block_hash,
            block_hash,
            original_value,
            merged_value: response.proposer_value,
            original_tx_count: original_payload.execution_payload.transactions.len(),
            merged_tx_count: response.execution_payload.transactions.len(),
            original_blob_count: original_payload.blobs_bundle.blobs.len(),
            merged_blob_count: original_payload.blobs_bundle.blobs.len() +
                response.appended_blobs.len(),
            builder_inclusions: response.builder_inclusions,
            trace,
        });

        trace!(%block_hash, "stored merged block in local cache");

        let mut merged_blobs_bundle = original_payload.blobs_bundle.as_ref().to_owned();
        append_merged_blobs(
            &mut merged_blobs_bundle,
            response.appended_blobs,
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

        // Store locally to serve header requests, keyed by the beneficiary/coinbase
        // address of the base block the merge was built from, so that a merge for one
        // builder's block can never be served in place of another builder's original bid.
        let coinbase = original_payload.execution_payload.fee_recipient;
        self.best_merged_blocks
            .insert(coinbase, BestMergedBlock { base_block_time_ms, bid: new_bid.clone() });

        record_step("prepare_merged_payload_for_storage", start_time.elapsed());

        // Return the payload entry to be stored for get payload calls
        Ok(new_bid)
    }
}

struct BestMergedBlock {
    base_block_time_ms: u64,
    bid: PayloadEntry,
}

/// Appends the merged blobs to the original blobs bundle.
#[timed]
fn append_merged_blobs(
    original_blobs_bundle: &mut BlobsBundle,
    appended_blobs: Vec<BlobWithMetadata>,
    max_blobs_per_block: usize,
) -> Result<(), PayloadMergingError> {
    for blob_data in appended_blobs {
        original_blobs_bundle
            .push_blob(blob_data.commitment, &blob_data.proofs, blob_data.blob, max_blobs_per_block)
            .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;
    }

    Ok(())
}

/// Checks whether the merged block kept the original builder's tx ordering completely
/// unchanged except for the payment tx (the last tx in the original block), with any
/// additional orders appended after it. When that's the case, this is logged since it
/// indicates the merge only affected the payment tx rather than tx ordering/content.
/// Returns whether the condition was detected (and thus logged).
fn log_if_only_payment_tx_changed(
    base_block_hash: B256,
    merged_block_hash: B256,
    builder_pubkey: &BlsPublicKeyBytes,
    original_txs: &Transactions,
    merged_txs: &Transactions,
) -> bool {
    let Some(payment_tx_index) = original_txs.len().checked_sub(1) else {
        return false;
    };

    if merged_txs.len() <= payment_tx_index {
        return false;
    }

    let prefix_unchanged = original_txs[..payment_tx_index] == merged_txs[..payment_tx_index];
    let payment_tx_changed = original_txs[payment_tx_index] != merged_txs[payment_tx_index];

    if !(prefix_unchanged && payment_tx_changed) {
        return false;
    }

    info!(
        %base_block_hash,
        %merged_block_hash,
        %builder_pubkey,
        prefix_tx_count = payment_tx_index,
        appended_tx_count = merged_txs.len() - original_txs.len(),
        original_payment_tx = %original_txs[payment_tx_index].0,
        merged_payment_tx = %merged_txs[payment_tx_index].0,
        "merge kept original builder tx ordering unchanged except for the payment tx"
    );
    true
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
