use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use alloy_primitives::{B256, U256};
use flux::spine::{SpineProducer, SpineProducers};
use flux_utils::SharedVector;
use helix_common::{
    BuilderInfo, RelayConfig,
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::{CACHE_SIZE, SimulatorMetrics},
};
use helix_database::handle::DbHandle;
use helix_types::{BlsPublicKeyBytes, HydrationCache, Slot, SubmissionVersion};
use rustc_hash::FxHashMap;
use tracing::{debug, info, warn};

use crate::{
    api::{FutureBidSubmissionResult, builder::error::BuilderApiError},
    auctioneer::{
        AuctioneerHandle, BlockMergeResponse,
        bid_adjustor::BidAdjustor,
        bid_sorter::BidSorter,
        block_merger::BlockMerger,
        types::{PayloadEntry, PendingPayload, SubmissionRef},
    },
    simulator::{BlockMergeResponse, SimInboundPayload, tile::SimulationResult},
    spine::{
        HelixSpineProducers,
        messages::{SubmissionResultWithRef, ToSimKind, ToSimMsg},
    },
};

// Context that is only valid for a given slot
// could also be in State::Sorting but keeping it here lets us avoid reallocating memory each slot
pub struct SlotContext {
    pub bid_slot: Slot,
    pub pending_payload: Option<PendingPayload>,
    pub bid_sorter: BidSorter,
    /// builder -> version
    pub version: FxHashMap<BlsPublicKeyBytes, SubmissionVersion>,
    pub hydration_cache: HydrationCache,
    pub payloads: FxHashMap<B256, PayloadEntry>,
    pub block_merger: BlockMerger,
}

pub struct Context<B: BidAdjustor> {
    pub chain_info: ChainInfo,
    pub config: RelayConfig,
    pub cache: LocalCache,
    pub unknown_builder_info: BuilderInfo,
    pub db: DbHandle,
    pub slot_context: SlotContext,
    pub bid_adjustor: B,
    pub completed_dry_run: bool,
    pub future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    pub auctioneer_handle: AuctioneerHandle,
    pub sim_inbound: Arc<SharedVector<SimInboundPayload>>,
    pub accept_optimistic: Arc<AtomicBool>,
    pub failsafe_triggered: Arc<AtomicBool>,
}

const EXPECTED_PAYLOADS_PER_SLOT: usize = 5000;
const EXPECTED_BUILDERS_PER_SLOT: usize = 200;

impl<B: BidAdjustor> Context<B> {
    // TODO: refactor to accept fewer parameters
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sim_inbound: Arc<SharedVector<SimInboundPayload>>,
        accept_optimistic: Arc<AtomicBool>,
        failsafe_triggered: Arc<AtomicBool>,
        db: DbHandle,
        bid_sorter: BidSorter,
        cache: LocalCache,
        bid_adjustor: B,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        auctioneer_handle: AuctioneerHandle,
    ) -> Self {
        let unknown_builder_info = BuilderInfo {
            collateral: U256::ZERO,
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };

        let block_merger = BlockMerger::new(0, chain_info.clone(), cache.clone(), config.clone());

        let slot_context = SlotContext {
            bid_slot: Slot::new(0),
            pending_payload: None,
            bid_sorter,
            version: FxHashMap::with_capacity_and_hasher(
                EXPECTED_BUILDERS_PER_SLOT,
                Default::default(),
            ),
            hydration_cache: HydrationCache::new(),
            payloads: FxHashMap::with_capacity_and_hasher(
                EXPECTED_PAYLOADS_PER_SLOT,
                Default::default(),
            ),
            block_merger,
        };

        Self {
            chain_info,
            cache,
            unknown_builder_info,
            slot_context,
            db,
            config,
            bid_adjustor,
            completed_dry_run: false,
            future_results,
            auctioneer_handle,
            sim_inbound,
            accept_optimistic,
            failsafe_triggered,
        }
    }

    pub fn builder_info(&self, builder: &BlsPublicKeyBytes) -> BuilderInfo {
        self.cache.get_builder_info(builder).unwrap_or_else(|| self.unknown_builder_info.clone())
    }

    /// 1. Check whether we should demote the builder, this is processed even if the result comes
    ///    after the slot has finished
    /// 2. Store simulation to DB
    pub fn handle_simulation_result(
        &mut self,
        result: SimulationResult,
        already_sent: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let (_id, result) = result;

        let Some(result) = result else {
            return;
        };

        let builder = *result.submission.builder_public_key();
        let block_hash = *result.submission.block_hash();

        let is_adjusted = self.payloads.get(&block_hash).is_some_and(|bid| bid.is_adjusted());

        if let Err(err) = result.result.as_ref() {
            if err.is_demotable() {
                if is_adjusted {
                    warn!(%builder, %block_hash, %err, "block simulation resulted in an error. Disabling adjustments...");
                    debug!(
                        "invalid adjusted block header: {:?}",
                        result.submission.execution_payload_ref().to_header(None, None)
                    );

                    if !self.cache.adjustments_enabled.load(Ordering::Relaxed) {
                        warn!(%block_hash, "adjustments already disabled");
                    } else {
                        SimulatorMetrics::disable_adjustments();
                        self.cache.adjustments_enabled.store(false, Ordering::Relaxed);
                        self.db.disable_adjustments(
                            block_hash,
                            self.cache.adjustments_failsafe_trigger.clone(),
                            self.cache.adjustments_enabled.clone(),
                        );
                    }
                } else if self.cache.demote_builder(&builder) {
                    warn!(%builder, %block_hash, %err, "Block simulation resulted in an error. Demoting builder...");

                    SimulatorMetrics::demotion_count();

                    let reason = err.to_string();
                    let bid_slot = result.submission.slot();
                    let failsafe_triggered = self.failsafe_triggered.clone();

                    self.db.db_demote_builder(
                        bid_slot.as_u64(),
                        builder,
                        block_hash,
                        reason,
                        failsafe_triggered,
                    );
                } else {
                    warn!(%err, %builder, %block_hash, "builder already demoted, skipping demotion");
                }
            }
        } else if is_adjusted {
            debug!(%builder, %block_hash,"adjusted block passed simulator validation!");
        }

        if !already_sent && !result.optimistic_version.is_optimistic() {
            send_submission_result(
                producers,
                &self.future_results,
                result.submission_ref,
                Err(BuilderApiError::SimOnNextSlot),
            );
        }
    }

    pub fn on_new_slot(&mut self, bid_slot: Slot, producers: &mut HelixSpineProducers) {
        self.bid_slot = bid_slot;
        if let Some(pending) = self.pending_payload.take() {
            let _ = pending
                .res_tx
                .send(Err(crate::api::proposer::ProposerApiError::NoExecutionPayloadFound));
        }
        self.completed_dry_run = false;
        self.bid_sorter.process_slot(bid_slot.as_u64());

        // record cache sizes before clearing
        CACHE_SIZE.with_label_values(&["payloads"]).set(self.payloads.len() as f64);
        CACHE_SIZE.with_label_values(&["submission_versions"]).set(self.version.len() as f64);
        CACHE_SIZE
            .with_label_values(&["hydration_builders"])
            .set(self.hydration_cache.builder_count() as f64);
        CACHE_SIZE
            .with_label_values(&["hydration_transactions"])
            .set(self.hydration_cache.tx_count() as f64);
        CACHE_SIZE
            .with_label_values(&["hydration_blobs"])
            .set(self.hydration_cache.blob_count() as f64);

        self.version.clear();
        self.hydration_cache.clear();

        producers.produce(ToSimMsg {
            kind: ToSimKind::NewSlot,
            ix: 0,
            bid_slot: bid_slot.as_u64(),
        });

        self.block_merger.on_new_slot(bid_slot.as_u64());
        self.bid_adjustor.on_new_slot(bid_slot.as_u64());
        self.auctioneer_handle.clear_inflight_payloads();

        if !self.payloads.is_empty() {
            // here we need to deallocate a lot of data, taking more than 1s on busy slots
            // this is not a big issue since it 's only at the beginning of the slot, but it blocks
            // the full event loop, which is not ideal. An alternative would be to use a
            // buffer and overwrite the buffer slots, keeping only a block hash -> index
            // map, however that would require us to estimate a hard upper limit on
            // payloads received, or risk causing a missed slot

            let payloads_to_drop = std::mem::replace(
                &mut self.payloads,
                FxHashMap::with_capacity_and_hasher(EXPECTED_PAYLOADS_PER_SLOT, Default::default()),
            );
            std::thread::spawn(move || {
                let to_drop = payloads_to_drop.len();
                let start = Instant::now();
                drop(payloads_to_drop);
                info!("dropped {} payloads in {:?}", to_drop, start.elapsed())
            });
        }
    }

    pub fn handle_merge_response(&mut self, response: BlockMergeResponse) {
        let block_hash = response.execution_payload.block_hash;
        let Some(original_payload) = self.payloads.get(&response.base_block_hash) else {
            warn!(%block_hash, "could not fetch original payload for merged block");
            return;
        };

        let original_payload_and_blobs = original_payload.payload_and_blobs();
        let builder_pubkey = *original_payload.bid_data_ref().builder_pubkey;

        //TODO: this function does a lot of work, should move that work away from the event loop
        let Some(payload) = self
            .block_merger
            .prepare_merged_payload_for_storage(
                response,
                original_payload_and_blobs,
                builder_pubkey,
            )
            .ok()
        else {
            warn!(%block_hash, "failed to prepare merged payload for storage");
            return;
        };
        self.payloads.insert(block_hash, payload);
    }
}

impl<B: BidAdjustor> Deref for Context<B> {
    type Target = SlotContext;

    fn deref(&self) -> &Self::Target {
        &self.slot_context
    }
}

impl<B: BidAdjustor> DerefMut for Context<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slot_context
    }
}

pub fn send_submission_result<P>(
    producers: &mut P,
    future_results: &Arc<SharedVector<FutureBidSubmissionResult>>,
    sub_ref: SubmissionRef,
    result: Result<(), BuilderApiError>,
) where
    P: SpineProducers + AsRef<SpineProducer<SubmissionResultWithRef>>,
{
    let result = SubmissionResultWithRef::new(sub_ref, result);
    match result.sub_ref {
        SubmissionRef::Http(future_ix) => {
            if let Some(future) = future_results.get(future_ix) {
                future.set(result);
            }
        }
        SubmissionRef::Tcp { .. } => producers.produce(result),
        SubmissionRef::Internal => {}
    }
}
