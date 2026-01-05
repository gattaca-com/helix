use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256};
use helix_common::{
    BuilderInfo, RelayConfig, chain_info::ChainInfo, local_cache::LocalCache,
    metrics::SimulatorMetrics, spawn_tracked, utils::alert_discord,
};
use helix_types::{BlsPublicKeyBytes, HydrationCache, Slot, SubmissionVersion};
use rustc_hash::FxHashMap;
use tracing::{error, info, warn};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        BlockMergeResponse, Event,
        bid_adjustor::BidAdjustor,
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

pub struct Context<B: BidAdjustor> {
    pub chain_info: ChainInfo,
    pub config: RelayConfig,
    pub cache: LocalCache,
    pub unknown_builder_info: BuilderInfo,
    pub db: Arc<PostgresDatabaseService>,
    pub slot_context: SlotContext,
    pub bid_adjustor: B,
    pub adjustments_enabled: Arc<AtomicBool>,
    pub adjustments_failsafe_trigger: Arc<AtomicBool>,
}

const EXPECTED_PAYLOADS_PER_SLOT: usize = 5000;
const EXPECTED_BUILDERS_PER_SLOT: usize = 200;

const DB_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const ADJUSTMENTS_DRY_RUN_INTERVAL: Duration = Duration::from_millis(500);

impl<B: BidAdjustor> Context<B> {
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sim_manager: SimulatorManager,
        db: Arc<PostgresDatabaseService>,
        bid_sorter: BidSorter,
        cache: LocalCache,
        bid_adjustor: B,
        auctioneer: crossbeam_channel::Sender<Event>,
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

        let adjustments_enabled = Arc::new(AtomicBool::new(false));
        let adjustments_failsafe_trigger = Arc::new(AtomicBool::new(false));

        Self::spawn_check_flag_task(
            db.clone(),
            adjustments_enabled.clone(),
            adjustments_failsafe_trigger.clone(),
        );

        Self::spawn_adjustments_dry_run_task(auctioneer, adjustments_enabled.clone());

        Self {
            chain_info,
            cache,
            unknown_builder_info,
            slot_context,
            db,
            config,
            bid_adjustor,
            adjustments_enabled,
            adjustments_failsafe_trigger,
        }
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
            if self.payloads.get(&block_hash).is_some_and(|bid| bid.is_adjusted()) {
                warn!(%builder, %block_hash, %err, "block simulation resulted in an error. Disabling adjustments...");

                if !self.adjustments_enabled.load(Ordering::Relaxed) {
                    warn!(%builder, %block_hash, %err, "adjustments already disabled");
                } else {
                    SimulatorMetrics::disable_adjustments();
                    self.adjustments_enabled.store(false, Ordering::Relaxed);

                    let db = self.db.clone();
                    let failsafe_trigger = self.adjustments_failsafe_trigger.clone();
                    let adjustments_enabled = self.adjustments_enabled.clone();
                    spawn_tracked!(async move {
                        if let Err(err) = db.disable_adjustments().await {
                            failsafe_trigger.store(true, Ordering::Relaxed);
                            adjustments_enabled.store(false, Ordering::Relaxed);
                            error!(%block_hash, %err, "failed to disable adjustments in database, pulling the failsafe trigger");
                            alert_discord(&format!(
                                "{} {} failed to disable adjustments in database, pulling the failsafe trigger",
                                err, block_hash
                            ));
                        }
                    });
                }
            } else if self.cache.demote_builder(&builder) {
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
                        alert_discord(&format!(
                            "{} {} {} failed to demote builder in database! Pausing all optmistic submissions",
                            builder, err, block_hash
                        ));
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
        self.bid_adjustor.on_new_slot(bid_slot.as_u64());

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

        let original_payload_and_blobs = original_payload.payload_and_blobs();
        let builder_pubkey = original_payload.bid_data_ref().builder_pubkey.clone();

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

    fn spawn_check_flag_task(
        db: Arc<PostgresDatabaseService>,
        flag: Arc<AtomicBool>,
        failsafe_triggered: Arc<AtomicBool>,
    ) {
        spawn_tracked!(async move {
            let mut interval = tokio::time::interval(DB_CHECK_INTERVAL);
            loop {
                interval.tick().await;

                if failsafe_triggered.load(Ordering::Relaxed) {
                    flag.store(false, Ordering::Relaxed);
                    return;
                }

                match db.check_adjustments_enabled().await {
                    Ok(value) => {
                        let previous = flag.swap(value, Ordering::Relaxed);
                        if previous != value {
                            tracing::info!(
                                "adjustments enabled flag changed from {} to {}",
                                previous,
                                value
                            );
                        }
                    }
                    Err(e) => tracing::error!("failed to check adjustments_enabled flag: {}", e),
                }
            }
        });
    }

    fn spawn_adjustments_dry_run_task(
        auctioneer: crossbeam_channel::Sender<Event>,
        adjustments_enabled: Arc<AtomicBool>,
    ) {
        spawn_tracked!(async move {
            let mut interval = tokio::time::interval(ADJUSTMENTS_DRY_RUN_INTERVAL);
            loop {
                interval.tick().await;

                if !adjustments_enabled.load(Ordering::Relaxed) {
                    return;
                }

                if let Err(e) = auctioneer.try_send(Event::DryRunAdjustments) {
                    error!("failed to send adjustments dry run request: {}", e);
                }
            }
        });
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
