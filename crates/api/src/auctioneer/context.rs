use std::{
    ops::{Deref, DerefMut},
    sync::{atomic::Ordering, Arc},
};

use alloy_primitives::{B256, U256};
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, metrics::SimulatorMetrics, spawn_tracked,
    BuilderInfo, RelayConfig,
};
use helix_database::DatabaseService;
use helix_types::{BlsPublicKeyBytes, HydrationCache, Slot};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{error, warn};

use crate::{
    auctioneer::{
        bid_sorter::BidSorter,
        simulator::manager::{SimulationResult, SimulatorManager},
        types::{PayloadEntry, PendingPayload},
    },
    Api,
};

// Context that is only valid for a given slot
// could also be in State::Sorting but keeping it here lets us avoid reallocating memory each slot
pub struct SlotContext {
    pub bid_slot: Slot,
    pub pending_payload: Option<PendingPayload>,
    pub bid_sorter: BidSorter,
    pub seen_block_hashes: FxHashSet<B256>,
    /// builder -> (last on_receive_ns, sequence number)
    pub sequence: FxHashMap<BlsPublicKeyBytes, (u64, Option<u64>)>,
    pub hydration_cache: HydrationCache,
    pub payloads: FxHashMap<B256, PayloadEntry>,
    pub sim_manager: SimulatorManager,
}

pub struct Context<A: Api> {
    pub chain_info: ChainInfo,
    pub _config: RelayConfig,
    pub cache: LocalCache,
    pub unknown_builder_info: BuilderInfo,
    pub db: Arc<A::DatabaseService>,
    pub slot_context: SlotContext,
}

impl<A: Api> Context<A> {
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sim_manager: SimulatorManager,
        db: Arc<A::DatabaseService>,
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

        let slot_context = SlotContext {
            sim_manager,
            bid_slot: Slot::new(0),
            pending_payload: None,
            bid_sorter,
            seen_block_hashes: FxHashSet::with_capacity_and_hasher(2000, Default::default()),
            sequence: FxHashMap::with_capacity_and_hasher(200, Default::default()),
            hydration_cache: HydrationCache::new(),
            payloads: FxHashMap::with_capacity_and_hasher(2000, Default::default()),
        };

        Self { chain_info, cache, unknown_builder_info, slot_context, db, _config: config }
    }

    pub fn builder_info(&self, builder: &BlsPublicKeyBytes) -> BuilderInfo {
        self.cache.get_builder_info(builder).unwrap_or_else(|| self.unknown_builder_info.clone())
    }

    /// 1. Check whether we should demote the builder, this is processed even if the result comes
    ///    after the slot has finished
    /// 2. Store simulation to DB
    pub fn handle_simulation_result(&mut self, result: SimulationResult) {
        let (id, result) = result;

        let paused_until = result.as_ref().and_then(|r| r.paused_until.clone());
        self.sim_manager.handle_task_response(id, paused_until);

        let Some(result) = result else {
            return;
        };

        let builder = *result.submission.builder_public_key();
        let block_hash = *result.submission.block_hash();

        if let Err(err) = result.result.as_ref() {
            if err.is_demotable() {
                if self.cache.demote_builder(&builder) {
                    warn!(%builder, %block_hash, %err, "Block simulation resulted in an error. Demoting builder...");

                    SimulatorMetrics::demotion_count();

                    let reason = err.to_string();
                    let bid_slot = result.submission.slot();
                    let failsafe_triggered = self.sim_manager.failsafe_triggered.clone();

                    let db = self.db.clone();
                    spawn_tracked!(async move {
                        if let Err(err) = db
                            .db_demote_builder(bid_slot.as_u64(), &builder, &block_hash, reason)
                            .await
                        {
                            failsafe_triggered.store(true, Ordering::Relaxed);
                            error!(%builder, %err, %block_hash, "failed to demote builder in database! Pausing all optmistic submissions");
                        }
                    });
                } else {
                    warn!(%err, %builder, %block_hash, "failed simulation with known error, skipping demotion");
                }
            };
        }

        let db = self.db.clone();
        spawn_tracked!(async move {
            if let Err(err) = db
                .store_block_submission(result.submission, result.trace, result.optimistic_version)
                .await
            {
                error!(%err, "failed to store block submission")
            }
        });
    }

    pub fn on_new_slot(&mut self, bid_slot: Slot) {
        self.bid_slot = bid_slot;
        if let Some(pending) = self.pending_payload.take() {
            let _ = pending
                .res_tx
                .send(Err(crate::proposer::ProposerApiError::NoExecutionPayloadFound));
        }
        self.bid_sorter.process_slot(bid_slot.as_u64());
        self.seen_block_hashes.clear();
        self.sequence.clear();
        self.hydration_cache.clear();
        self.payloads.clear();
        self.sim_manager.on_new_slot(bid_slot.as_u64());
    }
}

impl<A: Api> Deref for Context<A> {
    type Target = SlotContext;

    fn deref(&self) -> &Self::Target {
        &self.slot_context
    }
}

impl<A: Api> DerefMut for Context<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slot_context
    }
}
