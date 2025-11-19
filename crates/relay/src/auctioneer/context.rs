use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use alloy_primitives::{B256, U256};
use helix_common::{
    BuilderInfo, RelayConfig, chain_info::ChainInfo, local_cache::LocalCache,
    metrics::SimulatorMetrics, spawn_tracked,
};
use helix_types::{BlsPublicKeyBytes, HydrationCache, Slot, SubmissionVersion};
use rustc_hash::FxHashMap;
use tracing::{error, info, warn};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        BlockMergeResponse,
        bid_sorter::BidSorter,
        block_merger::BlockMerger,
        simulator::manager::{SimulationResult, SimulatorManager},
        types::{PayloadEntry, PendingPayload},
    },
    database::postgres::postgres_db_service::PostgresDatabaseService,
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
    pub sim_manager: SimulatorManager,
    pub block_merger: BlockMerger,
}

pub struct Context {
    pub chain_info: ChainInfo,
    pub config: RelayConfig,
    pub cache: LocalCache,
    pub unknown_builder_info: BuilderInfo,
    pub db: Arc<PostgresDatabaseService>,
    pub slot_context: SlotContext,
}

const EXPECTED_PAYLOADS_PER_SLOT: usize = 5000;
const EXPECTED_BUILDERS_PER_SLOT: usize = 200;

impl Context {
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sim_manager: SimulatorManager,
        db: Arc<PostgresDatabaseService>,
        bid_sorter: BidSorter,
        cache: LocalCache,
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
            sim_manager,
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

        Self { chain_info, cache, unknown_builder_info, slot_context, db, config }
    }

    pub fn builder_info(&self, builder: &BlsPublicKeyBytes) -> BuilderInfo {
        self.cache.get_builder_info(builder).unwrap_or_else(|| self.unknown_builder_info.clone())
    }

    /// 1. Check whether we should demote the builder, this is processed even if the result comes
    ///    after the slot has finished
    /// 2. Store simulation to DB
    pub fn handle_simulation_result(&mut self, result: SimulationResult) {
        let (id, result) = result;

        let paused_until = result.as_ref().and_then(|r| r.paused_until);
        self.sim_manager.handle_task_response(id, paused_until);

        let Some(result) = result else {
            return;
        };

        let builder = *result.submission.builder_public_key();
        let block_hash = *result.submission.block_hash();

        if let Err(err) = result.result.as_ref() &&
            err.is_demotable()
        {
            if self.cache.demote_builder(&builder) {
                warn!(%builder, %block_hash, %err, "Block simulation resulted in an error. Demoting builder...");

                SimulatorMetrics::demotion_count();

                let reason = err.to_string();
                let bid_slot = result.submission.slot();
                let failsafe_triggered = self.sim_manager.failsafe_triggered.clone();

                let db = self.db.clone();
                spawn_tracked!(async move {
                    if let Err(err) =
                        db.db_demote_builder(bid_slot.as_u64(), &builder, &block_hash, reason).await
                    {
                        failsafe_triggered.store(true, Ordering::Relaxed);
                        error!(%builder, %err, %block_hash, "failed to demote builder in database! Pausing all optmistic submissions");
                    }
                });
            } else {
                warn!(%err, %builder, %block_hash, "builder already demoted, skipping demotion");
            }
        };

        let db = self.db.clone();
        spawn_tracked!(async move {
            if let Err(err) = db
                .store_block_submission(result.submission, result.trace, result.optimistic_version)
                .await
            {
                error!(%err, "failed to store block submission")
            }
        });

        if let Some(res_tx) = result.res_tx {
            // submission was initially valid but by the time sim finished the slot already
            // progressed
            let _ = res_tx.send(Err(BuilderApiError::SimOnNextSlot));
        }
    }

    pub fn on_new_slot(&mut self, bid_slot: Slot) {
        self.bid_slot = bid_slot;
        if let Some(pending) = self.pending_payload.take() {
            let _ = pending
                .res_tx
                .send(Err(crate::api::proposer::ProposerApiError::NoExecutionPayloadFound));
        }
        self.bid_sorter.process_slot(bid_slot.as_u64());
        self.version.clear();
        self.hydration_cache.clear();
        self.sim_manager.on_new_slot(bid_slot.as_u64());
        self.block_merger.on_new_slot(bid_slot.as_u64());

        // here we need to deallocate a lot of data, taking more than 1s on busy slots
        // this is not a big issue since it 's only at the beginning of the slot, but it blocks the
        // full event loop, which is not ideal. An alternative would be to use a buffer and
        // overwrite the buffer slots, keeping only a block hash -> index map, however that
        // would require us to estimate a hard upper limit on payloads received, or risk causing a
        // missed slot
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

    pub fn handle_merge_response(&mut self, response: BlockMergeResponse) {
        let block_hash = response.execution_payload.block_hash;
        let Some(original_payload) = self.payloads.get(&response.base_block_hash) else {
            warn!(%block_hash, "could not fetch original payload for merged block");
            return;
        };

        let original_payload_and_blobs = original_payload.payload_and_blobs.clone();
        let builder_pubkey = original_payload.bid_data.builder_pubkey;

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

impl Deref for Context {
    type Target = SlotContext;

    fn deref(&self) -> &Self::Target {
        &self.slot_context
    }
}

impl DerefMut for Context {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slot_context
    }
}
