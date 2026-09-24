use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256};
use flux::{
    spine::{SpineProducer, SpineProducers},
    timing::Nanos,
};
use flux_profiler::timed;
use flux_utils::SharedVector;
use helix_common::{
    BuilderInfo, RelayConfig,
    alerts::{AlertManager, format_demotion_alert},
    chain_info::ChainInfo,
    http::client::{HttpClient, PendingResponse},
    is_local_dev,
    local_cache::LocalCache,
    metrics::{CACHE_SIZE, MERGE_SIM, SimulatorMetrics},
    spawn_tracked,
    utils::{discord_payload, discord_webhook_url, utcnow_ms},
};
use helix_database::handle::DbHandle;
use helix_operator::OperatorPubSub;
use helix_types::{
    BlsPublicKeyBytes, Demotion, HydrationCache, OperatorMessage, Slot, SubmissionVersion,
};
use rustc_hash::FxHashMap;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    SubmissionDataWithSpan,
    api::{
        FutureBidSubmissionResult, builder::error::BuilderApiError, proposer::GloasBuilderIdentity,
    },
    auctioneer::{
        AuctioneerHandle, BlockMergeResponse,
        bid_adjustor::BidAdjustor,
        bid_sorter::BidSorter,
        block_merger::BlockMerger,
        builder_preferences::BuilderPreferencesStore,
        types::{PayloadEntry, PendingPayload, SlotData, SubmissionRef, SubmissionRefKind},
    },
    simulator::{
        MergedSimulationResultInner, MergedValidationRequest, Simulators, ValidationResult,
    },
    spine::{HelixSpineProducers, messages::SubmissionResultWithRef},
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
    pub decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    pub future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    pub auctioneer_handle: AuctioneerHandle,
    pub sims: Simulators,
    pub block_merging_enabled: Arc<AtomicBool>,
    pub failsafe_triggered: Arc<AtomicBool>,
    pub alert_manager: Arc<AlertManager>,
    pub operator_api: Option<Arc<OperatorPubSub>>,
    http: HttpClient,
    /// Resolved at startup: the webhook's DNS lookup blocks, so it must stay off the loop.
    discord_addr: Option<SocketAddr>,
    discord_alert: Option<PendingResponse>,
    pub builder_preferences: BuilderPreferencesStore,
    pub gloas_builder_identity: Arc<GloasBuilderIdentity>,
}

const EXPECTED_PAYLOADS_PER_SLOT: usize = 5000;
const EXPECTED_BUILDERS_PER_SLOT: usize = 200;

impl<B: BidAdjustor> Context<B> {
    // TODO: refactor to accept fewer parameters
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sims: Simulators,
        block_merging_enabled: Arc<AtomicBool>,
        failsafe_triggered: Arc<AtomicBool>,
        db: DbHandle,
        bid_sorter: BidSorter,
        cache: LocalCache,
        bid_adjustor: B,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        auctioneer_handle: AuctioneerHandle,
        alert_manager: Arc<AlertManager>,
        operator_api: Option<Arc<OperatorPubSub>>,
        gloas_builder_identity: Arc<GloasBuilderIdentity>,
    ) -> Self {
        // Local dev builders have random keys, so none is ever in config.
        let local_dev = is_local_dev();
        let unknown_builder_info = BuilderInfo {
            collateral: if local_dev { U256::MAX } else { U256::ZERO },
            is_optimistic: local_dev,
            is_optimistic_for_regional_filtering: local_dev,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };

        let block_merger = BlockMerger::new(0, cache.clone(), config.clone());

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
            decoded,
            future_results,
            auctioneer_handle,
            sims,
            block_merging_enabled,
            failsafe_triggered,
            alert_manager,
            operator_api,
            http: HttpClient::new().expect("http client"),
            discord_addr: discord_webhook_url().and_then(|url| {
                HttpClient::resolve(url)
                    .inspect_err(|err| error!(%err, "failed to resolve discord webhook"))
                    .ok()
            }),
            discord_alert: None,
            builder_preferences: BuilderPreferencesStore::default(),
            gloas_builder_identity,
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
        result: ValidationResult,
        already_sent: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let (_id, result) = result;

        let Some(result) = result else {
            return;
        };

        if let Some(bid) = &result.bid {
            let builder = bid.builder_pubkey;
            let block_hash = bid.block_hash;
            let is_adjusted = self.payloads.get(&block_hash).is_some_and(|b| b.is_adjusted());

            if let Err(err) = result.result.as_ref() {
                if err.is_demotable() {
                    if is_adjusted {
                        warn!(%builder, %block_hash, %err, "block simulation resulted in an error. Disabling adjustments...");

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
                    } else {
                        let reason = err.to_string();
                        let bid_slot = bid.slot;
                        self.handle_builder_demotion(
                            bid_slot.into(),
                            builder,
                            block_hash,
                            reason,
                            true,
                        );
                    }
                }
            } else if is_adjusted {
                debug!(%builder, %block_hash, "adjusted block passed simulator validation!");
            }
        }

        if !already_sent && !result.optimistic_version.is_optimistic() {
            send_submission_result(
                producers,
                &self.future_results,
                result.submission_id,
                result.submission_ref,
                Err(BuilderApiError::SimOnNextSlot),
            );
        }
    }

    #[timed]
    pub fn on_new_slot(&mut self, bid_slot: Slot) {
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

        self.sims.on_new_slot(bid_slot.as_u64());

        let merged_blocks = self.cache.get_merged_blocks();
        if !merged_blocks.is_empty() {
            self.db.save_merged_blocks(merged_blocks);
            self.cache.clear_merged_blocks();
        }

        self.block_merger.on_new_slot(bid_slot.as_u64());
        self.bid_adjustor.on_new_slot(bid_slot.as_u64());
        self.builder_preferences.on_new_slot(bid_slot.as_u64());
        self.auctioneer_handle.clear_inflight_payloads();
        self.decoded.clear();

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
            let dealloc_core = self.config.cores.dealloc;
            std::thread::spawn(move || {
                // Unpinned, this lands on whatever core the OS picks -- including a
                // tokio worker's, which every other hot thread here is pinned away from.
                if let Some(core) = dealloc_core {
                    helix_common::utils::pin_thread_to_core(core);
                }
                let to_drop = payloads_to_drop.len();
                let start = Instant::now();
                drop(payloads_to_drop);
                info!("dropped {} payloads in {:?}", to_drop, start.elapsed())
            });
        }
    }

    pub fn on_merged_sim_result(&mut self, inner: &MergedSimulationResultInner) {
        let err = match &inner.result {
            Ok(()) => return MERGE_SIM.with_label_values(&["ok"]).inc(),
            Err(err) if !err.is_merge_builder_fault() => {
                return MERGE_SIM.with_label_values(&["failed_infra"]).inc();
            }
            Err(err) => err,
        };
        MERGE_SIM.with_label_values(&["failed_builder"]).inc();
        let block_hash = inner.block_hash;

        self.block_merging_enabled.store(false, Ordering::Relaxed);
        let endpoint = self
            .config
            .block_merging_config
            .tcp
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |tcp| tcp.builder.addr.to_string());
        error!(
            %block_hash,
            %err,
            %endpoint,
            "merged block simulation failed, disabling block merging"
        );
        let message = format!(
            "CRITICAL: block merging disabled -- merged block simulation failed for block \
             {block_hash:#x} from merge builder {endpoint} ({err})"
        );
        let Some((webhook_url, content)) = discord_payload(&message) else { return };
        let Some(addr) = self.discord_addr else {
            error!("discord webhook address unresolved, dropping alert");
            return;
        };
        let body = serde_json::to_vec(&content).expect("string map serializes");
        self.discord_alert = self
            .http
            .post_to(webhook_url, addr, body.into())
            .map(|req| req.with_timeout(Duration::from_secs(10)))
            .inspect_err(|err| error!(%err, "failed to send discord alert"))
            .ok();
    }

    pub fn poll_discord_alert(&mut self) {
        let Some(req) = self.discord_alert.as_mut() else { return };
        let Poll::Ready(res) = req.poll_bytes() else { return };
        match res {
            Ok((status, _)) if (200..300).contains(&status) => {}
            Ok((status, body)) => {
                error!(status, body = %String::from_utf8_lossy(&body), "discord alert rejected")
            }
            Err(err) => error!(%err, "failed to send discord alert"),
        }
        self.discord_alert = None;
    }

    pub fn handle_merge_response(&mut self, response: &BlockMergeResponse) {
        let block_hash = response.execution_payload.block_hash;
        let Some(original_payload) = self.payloads.get(&response.base_block_hash) else {
            warn!(%block_hash, "could not fetch original payload for merged block");
            return;
        };

        let original_payload_and_blobs = original_payload.payload_and_blobs();
        let original_value = *original_payload.value();
        let builder_pubkey = *original_payload.bid_data_ref().builder_pubkey;

        //TODO: this function does a lot of work, should move that work away from the event loop
        let Some(payload) = self
            .block_merger
            .prepare_merged_payload_for_storage(
                response.clone(),
                original_payload_and_blobs,
                original_value,
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

pub(crate) fn merged_validation_request(
    response: &BlockMergeResponse,
    slot_data: &SlotData,
) -> Option<MergedValidationRequest> {
    let parent_beacon_block_root = slot_data
        .attrs_for_submission(
            &response.execution_payload.parent_hash,
            &response.execution_payload.prev_randao,
        )?
        .parent_root();
    Some(MergedValidationRequest {
        submission_id: Uuid::new_v4(),
        base_block_hash: response.base_block_hash,
        slot: slot_data.bid_slot.as_u64(),
        parent_beacon_block_root,
        proposer_fee_recipient: slot_data
            .registration_data
            .entry
            .registration
            .message
            .fee_recipient,
        registered_gas_limit: slot_data.registration_data.entry.registration.message.gas_limit,
        apply_blacklist: slot_data.registration_data.entry.preferences.filtering.is_regional(),
        inclusion_list: slot_data.il.clone().unwrap_or_default(),
        receive_ns: Nanos::now().0,
    })
}

impl<B: BidAdjustor> Context<B> {
    pub fn handle_builder_demotion(
        &mut self,
        slot: Slot,
        builder_pubkey: BlsPublicKeyBytes,
        block_hash: B256,
        reason: String,
        from_simulation: bool,
    ) {
        if self.cache.demote_builder(&builder_pubkey) {
            if from_simulation {
                warn!(%builder_pubkey, %block_hash, reason, "Block simulation resulted in an error. Demoting builder...");
                SimulatorMetrics::demotion_count();
            } else {
                warn!(%builder_pubkey, %block_hash, "builder demoted due to block validation failure");
            }

            let db = self.db.clone();
            let failsafe = self.failsafe_triggered.clone();
            let slot_u64 = slot.as_u64();
            let alert_manager = self.alert_manager.clone();
            let network = self.config.website.network_name.clone();
            let region = self.config.postgres.region_name.clone();
            let builder_id =
                self.cache.get_builder_info(&builder_pubkey).and_then(|i| i.builder_id);

            if let Some(operator_api) = self.operator_api.as_ref() &&
                let Err(e) = operator_api.try_send(
                    builder_id.clone(),
                    OperatorMessage::Demotion(Demotion {
                        ts_ms: utcnow_ms(),
                        slot: slot_u64,
                        builder_pubkey,
                        block_hash,
                        reason_msg: reason.as_bytes().to_vec(),
                    }),
                )
            {
                tracing::error!(
                    ?e,
                    "failed to send operator demotion message for {:?}",
                    builder_pubkey
                );
            }

            let r = reason.clone();
            spawn_tracked!(async move {
                db.db_demote_builder(slot_u64, builder_pubkey, block_hash, r, failsafe)
            });

            let builder_id = builder_id.unwrap_or_default();
            let token = self.alert_manager.generate_token(builder_pubkey);
            let message = format_demotion_alert(
                slot_u64,
                &network,
                &region,
                &builder_pubkey,
                &builder_id,
                &block_hash,
                &reason,
            );
            debug!(%message, "sending demotion alert");
            alert_manager.send_demotion(&message, &token, &builder_id);
        } else {
            warn!(%reason, %builder_pubkey, %block_hash, "builder already demoted, skipping demotion");
        }
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
    submission_id: Uuid,
    sub_ref: SubmissionRef,
    result: Result<(), BuilderApiError>,
) where
    P: SpineProducers + AsRef<SpineProducer<SubmissionResultWithRef>>,
{
    let result = SubmissionResultWithRef::new(submission_id, sub_ref, result);
    match result.sub_ref.kind {
        SubmissionRefKind::Http => {
            let future_result_id = result.sub_ref.id;
            if let Some(future) = future_results.get(future_result_id) {
                future.set(result);
            } else {
                tracing::warn!(
                    future_result_id,
                    "submission result dropped: no future found (connection may have closed)"
                );
            }
        }
        SubmissionRefKind::Tcp => producers.produce(result),
        SubmissionRefKind::Internal => {}
    }
}
