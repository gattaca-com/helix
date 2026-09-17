use std::{
    self,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use flux::{
    spine::SpineProducers as _,
    tile::{Tile, TileName},
    timing::Nanos,
};
use flux_profiler::timed;
use flux_utils::SharedVector;
use helix_common::{
    SimulatorConfig, SubmissionTrace,
    api::builder_api::InclusionListWithMetadata,
    bid_submission::OptimisticVersion,
    chain_info::ChainInfo,
    is_local_dev,
    metrics::SimulatorMetrics,
    record_submission_step,
    simulator::{
        BlockSimError, JsonValidationRequest, MergedJsonValidationRequest,
        SszMergedValidationRequest, SszValidationRequest,
    },
    spawn_tracked,
    utils::avg_duration,
    validator_preferences::{Filtering, ValidatorPreferences},
};
use helix_types::{
    BidTrace, BlsPublicKeyBytes, BlsSignatureBytes, SignedBidSubmission, SimHydrationCache,
    Submission,
};
use rustc_hash::FxHashMap;
use ssz::Encode as _;
use tracing::{debug, error, info, warn};

use crate::{
    HelixSpine, SimRequest, ValidationRequest,
    auctioneer::Bid,
    bid_decoder::SubmissionDataWithSpan,
    simulator::{BlockMergeResponse, MergedValidationRequest, SimResult, client::SimulatorClient},
    spine::{
        HelixSpineProducers,
        messages::{FromSimMsg, ToSimKind, ToSimMsg},
    },
};

pub struct SimulatorTile {
    simulators: Vec<SimEntry>,
    /// Indices of simulators with an SSZ endpoint — static after construction.
    ssz_sim_indices: Vec<usize>,
    requests: PendingRequests,
    priority_requests: PendingRequests,
    merge_requests: PendingMergeRequests,
    last_bid_slot: u64,
    local_telemetry: LocalTelemetry,
    /// Per-simulator counters for the current slot, indexed like `simulators`.
    sim_slot_stats: Vec<SimSlotStats>,
    /// Internal channel: async tasks notify the sim tile when work completes.
    task_tx: crossbeam_channel::Sender<SimTileInternalEvent>,
    rx: crossbeam_channel::Receiver<SimTileInternalEvent>,
    sim_requests: Arc<SharedVector<SimRequest>>,
    sim_results: Arc<SharedVector<SimResult>>,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    merged_blocks: Arc<SharedVector<BlockMergeResponse>>,
    hydration_cache: SimHydrationCache,
    chain_info: ChainInfo,
    /// If we have any synced simulator
    pub accept_optimistic: Arc<AtomicBool>,
    /// If we failed to demote a builder in the DB
    pub failsafe_triggered: Arc<AtomicBool>,
}

impl Tile<HelixSpine> for SimulatorTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        // Process internal task-completion events (async tasks → sim tile).
        // Collect first to release the borrow on self.rx before calling &mut self methods.
        let events: Vec<SimTileInternalEvent> = self.rx.try_iter().collect();
        for event in events {
            match event {
                SimTileInternalEvent::TaskDone { id, error, result_ix, elapsed } => {
                    self.handle_task_response(
                        id,
                        error,
                        result_ix,
                        elapsed,
                        &mut adapter.producers,
                    );
                }
                SimTileInternalEvent::SyncStatus { id, reported } => {
                    self.handle_sync_status(id, reported);
                }
            }
        }

        // Consume inbound spine messages from the auctioneer.
        adapter.consume(|msg: ToSimMsg, producers| match msg.kind {
            ToSimKind::Request => match self.sim_requests.get(msg.ix) {
                Some(payload) => match payload.as_ref() {
                    SimRequest::Validate { req, fast_track } => {
                        self.handle_sim_request((**req).clone(), *fast_track, producers);
                    }
                    SimRequest::ValidateMerged(req) => {
                        self.handle_merge_sim_request((**req).clone(), producers);
                    }
                },
                None => error!(?msg, "sim inbound payload not found"),
            },
            ToSimKind::NewSlot => {
                self.on_new_slot(msg.bid_slot);
            }
            ToSimKind::FeedCache => {
                self.handle_feed_cache(msg.ix, msg.bid_slot);
            }
        });
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate("simulator")
    }
}

impl SimulatorTile {
    pub fn create(
        configs: Vec<SimulatorConfig>,
        sim_requests: Arc<SharedVector<SimRequest>>,
        sim_results: Arc<SharedVector<SimResult>>,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        merged_blocks: Arc<SharedVector<BlockMergeResponse>>,
        chain_info: ChainInfo,
        failsafe_triggered: Arc<AtomicBool>,
    ) -> (Arc<AtomicBool>, Arc<AtomicBool>, Self) {
        let (task_tx, rx) = crossbeam_channel::bounded(512);

        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let simulators: Vec<_> = configs
            .into_iter()
            .map(|config| SimEntry::new(SimulatorClient::new(client.clone(), config)))
            .collect();

        let requests = PendingRequests::with_capacity(200);
        let priority_requests = PendingRequests::with_capacity(30);
        let merge_requests = PendingMergeRequests::with_capacity(30);

        if !is_local_dev() {
            let clients: Vec<SimulatorClient> =
                simulators.iter().map(|e| e.client.clone()).collect();
            spawn_tracked!({
                let sync_tx = task_tx.clone();
                async move {
                    loop {
                        for (id, simulator) in clients.iter().enumerate() {
                            let reported = simulator.is_synced().await.ok();
                            if sync_tx
                                .try_send(SimTileInternalEvent::SyncStatus { id, reported })
                                .is_err()
                            {
                                error!("failed to send sync status to sim tile");
                            }
                            SimulatorMetrics::simulator_sync(
                                simulator.endpoint(),
                                reported.unwrap_or(false),
                            );
                        }

                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            });
        }

        let accept_optimistic = Arc::new(AtomicBool::new(true));

        let ssz_sim_indices: Vec<usize> = simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.client.ssz_url.is_some())
            .map(|(i, _)| i)
            .collect();

        let sim_slot_stats = vec![SimSlotStats::default(); simulators.len()];

        let tile = Self {
            simulators,
            ssz_sim_indices,
            requests,
            priority_requests,
            merge_requests,
            last_bid_slot: 0,
            local_telemetry: LocalTelemetry::default(),
            sim_slot_stats,
            task_tx,
            rx,
            sim_requests,
            sim_results,
            decoded,
            merged_blocks,
            hydration_cache: SimHydrationCache::new(),
            chain_info,
            accept_optimistic: accept_optimistic.clone(),
            failsafe_triggered: failsafe_triggered.clone(),
        };

        (accept_optimistic, failsafe_triggered, tile)
    }

    fn handle_sync_status(&mut self, id: usize, reported: Option<bool>) {
        self.simulators[id].record_sync(reported);
        let now = Instant::now();
        let new = self.simulators.iter().any(|s| s.can_simulate_light(now));
        let prev = self.accept_optimistic.load(Ordering::Relaxed);
        if new != prev {
            warn!(prev, new, "changing accept_optimistic simulation status");
        }
        self.accept_optimistic.store(new, Ordering::Relaxed);
    }

    #[timed]
    fn handle_sim_request(
        &mut self,
        req: crate::simulator::ValidationRequest,
        fast_track: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let Some(decoded_data) = self.decoded.get(req.decoded_ix) else {
            error!(ix = req.decoded_ix, "decoded submission not found in ring");
            let result_ix =
                self.sim_results.push(SimResult::Validate((0, Some(infra_error(&req)))));
            producers.produce(FromSimMsg { ix: result_ix });
            return;
        };
        let builder_pubkey = *decoded_data.submission_data.submission.builder_pubkey();
        assert_eq!(decoded_data.submission_data.submission.bid_slot(), self.last_bid_slot);

        // Before deciding whether to simulate or queue: a queued submission would
        // otherwise leave no trace in the cache, and the next submission to
        // reference its transactions could not be hydrated.
        self.feed_cache(&decoded_data.submission_data.submission);

        self.local_telemetry.sims_reqs += 1;

        let sim_id = self.select_simulator();

        if let Some(id) = sim_id {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_sim(id, req)
        } else {
            self.local_telemetry.queued += 1;
            // Every request feeds the cache on arrival, so an evicted one has
            // already contributed its transactions.
            if fast_track {
                self.priority_requests.store(req, builder_pubkey, &mut self.local_telemetry);
            } else {
                self.requests.store(req, builder_pubkey, &mut self.local_telemetry);
            }
        }
    }

    #[timed]
    fn handle_merge_sim_request(
        &mut self,
        req: MergedValidationRequest,
        producers: &mut HelixSpineProducers,
    ) {
        if self.merged_blocks.get(req.merged_block_ix).is_none() {
            error!(ix = req.merged_block_ix, "merged block not found in ring");
            let result_ix = self
                .sim_results
                .push(SimResult::ValidateMerged((0, Some(infra_merge_error(&req)))));
            producers.produce(FromSimMsg { ix: result_ix });
            return;
        }

        self.local_telemetry.sims_reqs += 1;

        if let Some(id) = self.next_client(Instant::now()) {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_merge_sim(id, req);
        } else {
            self.local_telemetry.queued += 1;
            self.merge_requests.store(req);
        }
    }

    fn handle_task_response(
        &mut self,
        id: usize,
        error: Option<BlockSimError>,
        result_ix: usize,
        elapsed: Option<Duration>,
        producers: &mut HelixSpineProducers,
    ) {
        let now = Instant::now();
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);

        match error {
            Some(err) => sim.record_failure(&err, now),
            None => {
                if let Some(elapsed) = elapsed {
                    sim.record_success(elapsed);
                }
            }
        }

        if let Some(elapsed) = elapsed {
            let stats = &mut self.sim_slot_stats[id];
            stats.count += 1;
            stats.total_time += elapsed;
        }

        producers.produce(FromSimMsg { ix: result_ix });

        if let Some(id) = self.next_client(now) {
            if let Some(req) = self.priority_requests.next_req().or(self.requests.next_req()) {
                self.local_telemetry.sims_sent_from_queue += 1;
                self.spawn_sim(id, req);
            } else if let Some(req) = self.merge_requests.next_req() {
                self.spawn_merge_sim(id, req);
            }
        }
    }

    /// Even when a submission is queued instead of simulated, its full
    /// transactions and blobs must enter the cache so later dehydrated
    /// submissions can resolve their references.
    fn feed_cache(&mut self, submission: &Submission) {
        if let Submission::Dehydrated(d) = submission {
            self.hydration_cache.feed(d);
        }
    }

    /// Feeds a submission the tile is never asked to simulate, such as one the auctioneer
    /// rejected in validation after its transactions entered the auctioneer's own cache.
    fn handle_feed_cache(&mut self, decoded_ix: usize, bid_slot: u64) {
        if bid_slot != self.last_bid_slot {
            return;
        }
        let Some(decoded_data) = self.decoded.get(decoded_ix) else {
            warn!(ix = decoded_ix, "decoded submission not found in ring, cannot feed cache");
            return;
        };
        self.feed_cache(&decoded_data.submission_data.submission);
    }

    #[timed]
    fn spawn_sim(&mut self, id: usize, req: ValidationRequest) {
        let Some(decoded_data) = self.decoded.get(req.decoded_ix) else {
            error!(ix = req.decoded_ix, "decoded submission not found in ring");
            // Balance pending so handle_task_response can route the next request.
            let sim = &mut self.simulators[id];
            sim.pending += 1;
            let result_ix =
                self.sim_results.push(SimResult::Validate((id, Some(infra_error(&req)))));
            let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                id,
                error: None,
                result_ix,
                elapsed: None,
            });
            return;
        };

        let (submission, tx_root) = match decoded_data.submission_data.submission.clone() {
            Submission::Full(s) => (s, None),
            Submission::Dehydrated(d) => {
                match self.hydration_cache.hydrate(d, self.chain_info.max_blobs_per_block()) {
                    Ok(h) => (h.submission, h.tx_root),
                    Err(e) => {
                        // Read the identity off the original rather than the clone
                        // `hydrate` consumed, so the hit path pays nothing for it.
                        let sub = &decoded_data.submission_data.submission;
                        let builder_pubkey = *sub.builder_pubkey();
                        error!(
                            %e,
                            %builder_pubkey,
                            slot = sub.bid_slot(),
                            block_hash = %sub.block_hash(),
                            "hydration failed in sim tile"
                        );
                        *self
                            .local_telemetry
                            .hydration_failures
                            .entry(builder_pubkey)
                            .or_default() += 1;
                        let sim = &mut self.simulators[id];
                        sim.pending += 1;
                        let result_ix = self
                            .sim_results
                            .push(SimResult::Validate((id, Some(hydration_error(&req)))));
                        let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                            id,
                            error: None,
                            result_ix,
                            elapsed: None,
                        });
                        return;
                    }
                }
            }
        };

        let version = decoded_data.submission_data.version;
        let trace = decoded_data.submission_data.trace;
        let submission_ref = decoded_data.submission_data.submission_ref;

        let sim = &mut self.simulators[id];
        let dispatch = if let Some(url) = &sim.client.ssz_url {
            SimDispatch::Ssz {
                to_send: sim.client.client.post(format!("{url}/validate")),
                ssz_url: url.clone(),
                http: sim.client.client.clone(),
            }
        } else {
            let fork = submission.fork_name();
            let Some((builder, method)) = sim.client.sim_request_builder(fork) else {
                warn!(%fork, "no validation RPC method for fork, dropping submission");
                sim.pending += 1;
                let result_ix = self.sim_results.push(SimResult::Validate((
                    id,
                    Some(SimulationResultInner {
                        submission_ref: req.submission_ref,
                        optimistic_version: req.optimistic_version(),
                        bid: None,
                        result: Err(BlockSimError::UnsupportedFork(fork)),
                    }),
                )));
                let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                    id,
                    error: None,
                    result_ix,
                    elapsed: None,
                });
                return;
            };
            SimDispatch::Json { to_send: builder, method: method.to_owned() }
        };
        sim.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(sim.pending);
        let timer = SimulatorMetrics::timer(sim.client.endpoint());
        let task_tx = self.task_tx.clone();
        let sim_results = self.sim_results.clone();
        spawn_tracked!(async move {
            let start_sim = Nanos::now();
            let block_hash = submission.block_hash();
            debug!(%block_hash, "sending simulation request");

            let optimistic_version = req.optimistic_version();
            SimulatorMetrics::sim_count(optimistic_version.is_optimistic());
            let (mut res, ssz_retry) = match dispatch {
                SimDispatch::Ssz { to_send, ssz_url, http } => {
                    let request = create_ssz_request(&req, &submission);
                    let res =
                        SimulatorClient::do_sim_request(&request, req.is_top_bid, to_send).await;
                    (res, Some((request, ssz_url, http)))
                }
                SimDispatch::Json { to_send, method } => {
                    let filtering =
                        if req.apply_blacklist { Filtering::Regional } else { Filtering::Global };
                    let json_req = JsonValidationRequest::new(
                        req.registered_gas_limit,
                        &submission,
                        ValidatorPreferences { filtering, ..Default::default() },
                        Some(req.parent_beacon_block_root),
                        Some(req.inclusion_list.clone()),
                    );
                    let res = SimulatorClient::do_json_sim_request(
                        &json_req,
                        req.is_top_bid,
                        &method,
                        to_send,
                    )
                    .await;
                    (res, None)
                }
            };

            // On cache miss, retry with full uncompressed SSZ so the simulator
            // can process the submission without a hydration cache entry.
            if matches!(res, Err(BlockSimError::HydrationMiss)) {
                debug!(%block_hash, "hydration miss — retrying with full SSZ");
                if let Some((request, ssz_url, http)) = ssz_retry {
                    let to_send = http.post(format!("{ssz_url}/validate"));
                    let mut retry_req = request.clone();
                    retry_req.signed_bid_submission = submission.as_ssz_bytes();
                    res =
                        SimulatorClient::do_sim_request(&retry_req, req.is_top_bid, to_send).await;
                } else {
                    res = Err(BlockSimError::RpcError);
                }
            }

            let time = timer.stop_and_record();

            debug!(%block_hash, time_secs = time, ?res, "simulation completed");

            SimulatorMetrics::sim_status(res.is_ok());

            if let Some(got) = tx_root {
                let expected = submission.transactions_root();
                if expected != got {
                    res = Err(BlockSimError::InvalidTxRoot { got, expected })
                }
            }

            record_submission_step("simulation", start_sim.elapsed());

            let error = res.as_ref().err().cloned();
            let bid = Bid::new(version, &submission);
            let inner = SimulationResultInner {
                submission_ref,
                result: res.map(|()| trace),
                bid: Some(bid),
                optimistic_version,
            };

            let result_ix = sim_results.push(SimResult::Validate((id, Some(inner))));
            let _ = task_tx.try_send(SimTileInternalEvent::TaskDone {
                id,
                error,
                result_ix,
                elapsed: Some(Duration::from_secs_f64(time)),
            });
        });
    }

    #[timed]
    fn spawn_merge_sim(&mut self, id: usize, req: MergedValidationRequest) {
        let Some(response) = self.merged_blocks.get(req.merged_block_ix) else {
            error!(ix = req.merged_block_ix, "merged block not found in ring");
            let sim = &mut self.simulators[id];
            sim.pending += 1;
            let result_ix = self
                .sim_results
                .push(SimResult::ValidateMerged((id, Some(infra_merge_error(&req)))));
            let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                id,
                error: None,
                result_ix,
                elapsed: None,
            });
            return;
        };

        let submission = match merged_block_to_submission(&response, &req) {
            Ok(submission) => submission,
            Err(err) => {
                let sim = &mut self.simulators[id];
                sim.pending += 1;
                let inner = MergedSimulationResultInner {
                    merged_block_ix: req.merged_block_ix,
                    result: Err(err),
                };
                let result_ix = self.sim_results.push(SimResult::ValidateMerged((id, Some(inner))));
                let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                    id,
                    error: None,
                    result_ix,
                    elapsed: None,
                });
                return;
            }
        };

        let base_payment_tx_index = response.base_payment_tx_index as u64;

        let sim = &mut self.simulators[id];
        let dispatch = if let Some(url) = &sim.client.ssz_url {
            MergedSimDispatch::Ssz(sim.client.client.post(format!("{url}/validate_merged")))
        } else {
            let (builder, method) = sim.client.merged_sim_request_builder();
            MergedSimDispatch::Json { to_send: builder, method: method.to_owned() }
        };
        sim.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(sim.pending);
        let timer = SimulatorMetrics::timer(sim.client.endpoint());
        let task_tx = self.task_tx.clone();
        let sim_results = self.sim_results.clone();
        let merged_block_ix = req.merged_block_ix;
        let apply_blacklist = req.apply_blacklist;
        let registered_gas_limit = req.registered_gas_limit;
        let parent_beacon_block_root = req.parent_beacon_block_root;
        let inclusion_list = req.inclusion_list.clone();
        spawn_tracked!(async move {
            let start_sim = Nanos::now();
            let block_hash = submission.execution_payload.block_hash;
            debug!(%block_hash, "sending merged block simulation request");

            SimulatorMetrics::sim_count(false);
            let res = match dispatch {
                MergedSimDispatch::Ssz(to_send) => {
                    let request = ssz_merged_request(
                        apply_blacklist,
                        registered_gas_limit,
                        parent_beacon_block_root,
                        inclusion_list,
                        &submission,
                        base_payment_tx_index,
                    );
                    SimulatorClient::do_sim_request(&request, false, to_send).await
                }
                MergedSimDispatch::Json { to_send, method } => {
                    let filtering =
                        if apply_blacklist { Filtering::Regional } else { Filtering::Global };
                    let json_req = MergedJsonValidationRequest {
                        base: JsonValidationRequest::new(
                            registered_gas_limit,
                            &submission,
                            ValidatorPreferences { filtering, ..Default::default() },
                            Some(parent_beacon_block_root),
                            Some(inclusion_list),
                        ),
                        base_payment_tx_index,
                    };
                    SimulatorClient::do_json_sim_request(&json_req, false, &method, to_send).await
                }
            };

            let time = timer.stop_and_record();
            debug!(%block_hash, time_secs = time, ?res, "merged block simulation completed");

            SimulatorMetrics::sim_status(res.is_ok());

            record_submission_step("merge_simulation", start_sim.elapsed());

            let error = res.as_ref().err().cloned();
            let inner = MergedSimulationResultInner { merged_block_ix, result: res };
            let result_ix = sim_results.push(SimResult::ValidateMerged((id, Some(inner))));
            let _ = task_tx.try_send(SimTileInternalEvent::TaskDone {
                id,
                error,
                result_ix,
                elapsed: Some(Duration::from_secs_f64(time)),
            });
        });
    }

    /// Selection priority:
    /// 1. Any SSZ-capable sim, lowest utilization (binary protocol)
    /// 2. Any sim, lowest utilization (JSON-RPC fallback)
    #[timed]
    fn select_simulator(&self) -> Option<usize> {
        self.select_simulator_at(Instant::now())
    }

    fn select_simulator_at(&self, now: Instant) -> Option<usize> {
        self.ssz_sim_indices
            .iter()
            .filter(|&&i| self.simulators[i].can_simulate_at(now))
            .min_by_key(|&&i| self.simulators[i].utilization(now))
            .copied()
            .or_else(|| self.next_client(now))
    }

    fn next_client(&self, now: Instant) -> Option<usize> {
        self.simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.can_simulate_at(now))
            .min_by_key(|(_, s)| s.utilization(now))
            .map(|(i, _)| i)
    }

    fn on_new_slot(&mut self, bid_slot: u64) {
        if self.last_bid_slot > 0 {
            self.report();
        }

        self.last_bid_slot = bid_slot;
        self.requests.clear();
        self.priority_requests.clear();
        self.merge_requests.clear();
        self.hydration_cache.clear();
    }

    fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);
        let queue_left = self.requests.reqs.len() + self.priority_requests.reqs.len();

        SimulatorMetrics::sim_mananger_count("sims_sent_immediately", tel.sims_sent_immediately);
        SimulatorMetrics::sim_mananger_count("sims_reqs_dropped", tel.sims_reqs_dropped);
        SimulatorMetrics::sim_mananger_count("stale_sim_reqs", tel.stale_sim_reqs);
        SimulatorMetrics::sim_manager_gauge("max_pending", tel.max_pending);
        SimulatorMetrics::sim_manager_gauge("max_in_flight", tel.max_in_flight);

        let now = Instant::now();
        let sim_report: Vec<_> = self
            .simulators
            .iter()
            .zip(self.sim_slot_stats.iter())
            .map(|(sim, stats)| {
                let avg = avg_duration(stats.total_time, stats.count);
                let health = sim.health(now);
                let endpoint = sim.client.endpoint();
                SimulatorMetrics::simulator_limit(endpoint, sim.limit);
                SimulatorMetrics::simulator_health(endpoint, health.gauge());
                SimulatorMetrics::simulator_slot_latency(endpoint, avg);
                format!(
                    "{endpoint}: count={}, avg={avg:?}, limit={}, health={}",
                    stats.count,
                    sim.limit,
                    health.label()
                )
            })
            .collect();
        self.sim_slot_stats.fill(SimSlotStats::default());

        let mut by_builder: Vec<_> = tel.hydration_failures.iter().collect();
        by_builder.sort_unstable_by_key(|(_, n)| std::cmp::Reverse(**n));
        let hydration_failures: Vec<String> =
            by_builder.iter().take(5).map(|(k, n)| format!("{k}={n}")).collect();
        let hydration_failures_total: u32 = tel.hydration_failures.values().sum();

        info!(
            bid_slot = self.last_bid_slot,
            hydration_failures_total,
            ?hydration_failures,
            sims_reqs = tel.sims_reqs,
            sims_sent_immediately = tel.sims_sent_immediately,
            queued = tel.queued,
            sims_sent_from_queue = tel.sims_sent_from_queue,
            sims_reqs_dropped = tel.sims_reqs_dropped,
            queue_left,
            stale_sim_reqs = tel.stale_sim_reqs,
            max_pending = tel.max_pending,
            max_in_flight = tel.max_in_flight,
            ?sim_report,
            "simulator slot stats"
        )
    }
}

/// An open breaker whose `until` has passed is the half-open state: it admits one probe.
#[derive(Clone, Copy)]
enum Breaker {
    Closed,
    Open { until: Instant, backoff: Duration },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimHealth {
    Ok,
    Probe,
    Open,
    Unsynced,
}

impl SimHealth {
    fn label(self) -> &'static str {
        match self {
            SimHealth::Ok => "ok",
            SimHealth::Probe => "probe",
            SimHealth::Open => "open",
            SimHealth::Unsynced => "unsynced",
        }
    }

    fn gauge(self) -> usize {
        match self {
            SimHealth::Ok => 0,
            SimHealth::Probe => 1,
            SimHealth::Open => 2,
            SimHealth::Unsynced => 3,
        }
    }
}

struct SimEntry {
    client: SimulatorClient,
    is_synced: bool,
    breaker: Breaker,
    /// Adaptive concurrency limit, bounded by `max_concurrent_tasks`.
    limit: usize,
    /// Consecutive sim faults, reset by any answer from the node.
    faults: usize,
    /// Consecutive failed sync polls, reset by any answer from the node.
    sync_failures: usize,
    /// Current number of pending tasks (validation or merging)
    pending: usize,
}

impl SimEntry {
    fn new(client: SimulatorClient) -> Self {
        let limit = client.config.max_concurrent_tasks.max(MIN_LIMIT);
        Self {
            client,
            is_synced: false,
            breaker: Breaker::Closed,
            limit,
            faults: 0,
            sync_failures: 0,
            pending: 0,
        }
    }

    fn ceiling(&self) -> usize {
        self.client.config.max_concurrent_tasks.max(MIN_LIMIT)
    }

    /// How many tasks this sim may hold now: none while the breaker is open, one probe once
    /// the backoff expires, otherwise the adaptive limit.
    fn effective_limit(&self, now: Instant) -> usize {
        match self.breaker {
            Breaker::Closed => self.limit,
            Breaker::Open { until, .. } if now < until => 0,
            Breaker::Open { .. } => 1,
        }
    }

    /// A lighter check to decide whether we should accept optimistic submissions
    fn can_simulate_light(&self, now: Instant) -> bool {
        self.is_synced && self.effective_limit(now) > 0
    }

    fn can_simulate_at(&self, now: Instant) -> bool {
        self.is_synced && self.pending < self.effective_limit(now)
    }

    fn health(&self, now: Instant) -> SimHealth {
        if !self.is_synced {
            return SimHealth::Unsynced;
        }
        match self.effective_limit(now) {
            0 => SimHealth::Open,
            1 if !matches!(self.breaker, Breaker::Closed) => SimHealth::Probe,
            _ => SimHealth::Ok,
        }
    }

    /// The share of the current limit in use, scaled so it sorts as an integer.
    fn utilization(&self, now: Instant) -> usize {
        match self.effective_limit(now) {
            0 => usize::MAX,
            limit => self.pending * UTILIZATION_SCALE / limit,
        }
    }

    /// The node answered. Close the breaker, then let the latency move the limit.
    fn record_success(&mut self, elapsed: Duration) {
        self.faults = 0;
        self.breaker = Breaker::Closed;
        self.limit = if elapsed <= LATENCY_TARGET {
            (self.limit + 1).min(self.ceiling())
        } else {
            (self.limit / 2).max(MIN_LIMIT)
        };
    }

    fn record_failure(&mut self, err: &BlockSimError, now: Instant) {
        if !err.is_sim_fault() {
            return;
        }

        self.faults += 1;
        self.limit = (self.limit / 2).max(MIN_LIMIT);

        self.breaker = match self.breaker {
            Breaker::Open { backoff, .. } => {
                let backoff = (backoff * 2).min(BREAKER_BACKOFF_MAX);
                Breaker::Open { until: now + backoff, backoff }
            }
            Breaker::Closed if self.faults >= SIM_FAULTS_TO_OPEN => {
                Breaker::Open { until: now + BREAKER_BACKOFF_START, backoff: BREAKER_BACKOFF_START }
            }
            Breaker::Closed => Breaker::Closed,
        };
    }

    /// `None` means the poll itself failed. `Some(false)` is the node's own answer that it
    /// is still syncing, which we trust at once.
    fn record_sync(&mut self, reported: Option<bool>) {
        match reported {
            Some(synced) => {
                self.sync_failures = 0;
                self.is_synced = synced;
            }
            None => {
                self.sync_failures += 1;
                if self.sync_failures >= SYNC_FAILURES_TO_UNSYNC {
                    self.is_synced = false;
                }
            }
        }
    }
}

pub(crate) const SIMULATOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A simulation slower than this means the node is over its comfortable load.
const LATENCY_TARGET: Duration = Duration::from_millis(300);
const MIN_LIMIT: usize = 1;
const UTILIZATION_SCALE: usize = 1_000;
const SIM_FAULTS_TO_OPEN: usize = 3;
const BREAKER_BACKOFF_START: Duration = Duration::from_secs(12);
const BREAKER_BACKOFF_MAX: Duration = Duration::from_secs(60);
const SYNC_FAILURES_TO_UNSYNC: usize = 3;

#[derive(Default)]
struct LocalTelemetry {
    sims_reqs: usize,
    sims_sent_immediately: usize,
    sims_reqs_dropped: usize,
    stale_sim_reqs: usize,
    // waiting to be sent
    max_pending: usize,
    // waiting for result
    max_in_flight: usize,
    /// Requests with no simulator free at intake, queued for later dispatch.
    /// `sims_reqs == sims_sent_immediately + queued`.
    queued: usize,
    /// Queued requests dispatched once a simulator freed up (queued ->
    /// sims_sent_from_queue, or evicted by a fresher request for the same
    /// builder -> sims_reqs_dropped, or still resident at slot end ->
    /// `queue_left`, read live from the queues rather than stored here).
    sims_sent_from_queue: usize,
    /// Hydration misses this slot, per builder: which builder's dehydrated
    /// submissions the sim tile could not rebuild. See gattaca-com/helix#563.
    hydration_failures: FxHashMap<BlsPublicKeyBytes, u32>,
}

pub type ValidationResult = (usize, Option<SimulationResultInner>);
#[derive(Clone)]
pub struct SimulationResultInner {
    pub submission_ref: crate::auctioneer::SubmissionRef,
    pub optimistic_version: OptimisticVersion,
    /// None for infra errors where simulation never ran (no decoded data available).
    pub bid: Option<Bid>,
    /// Ok carries the trace; Err carries the simulation failure.
    pub result: Result<SubmissionTrace, BlockSimError>,
}

pub type MergedSimulationResult = (usize, Option<MergedSimulationResultInner>);
#[derive(Clone)]
pub struct MergedSimulationResultInner {
    pub merged_block_ix: usize,
    /// Ok on a valid merged block; Err carries the simulation failure.
    pub result: Result<(), BlockSimError>,
}

enum SimDispatch {
    Ssz { to_send: reqwest::RequestBuilder, ssz_url: String, http: reqwest::Client },
    Json { to_send: reqwest::RequestBuilder, method: String },
}

/// Merged-block counterpart of [`SimDispatch`]: no hydration-miss retry (merged blocks are
/// always full, never dehydrated), so it doesn't need `SimDispatch::Ssz`'s extra fields.
enum MergedSimDispatch {
    Ssz(reqwest::RequestBuilder),
    Json { to_send: reqwest::RequestBuilder, method: String },
}

/// Internal-only events: async task → sim tile (not tile-to-tile).
pub(super) enum SimTileInternalEvent {
    /// `elapsed` is `None` for infra errors where no request was actually sent
    /// (e.g. decoded submission or hydration missing).
    TaskDone {
        id: usize,
        error: Option<BlockSimError>,
        result_ix: usize,
        elapsed: Option<Duration>,
    },
    SyncStatus {
        id: usize,
        reported: Option<bool>,
    },
}

#[derive(Default, Clone, Copy)]
struct SimSlotStats {
    count: u32,
    total_time: Duration,
}

/// Pending requests, we only keep the last one for each builder.
struct PendingRequests {
    reqs: Vec<(crate::simulator::ValidationRequest, BlsPublicKeyBytes)>,
}

impl PendingRequests {
    fn with_capacity(capacity: usize) -> Self {
        Self { reqs: Vec::with_capacity(capacity) }
    }

    /// Returns the evicted request if a newer one replaced it.
    fn store(
        &mut self,
        req: crate::simulator::ValidationRequest,
        builder_pubkey: BlsPublicKeyBytes,
        local_telemetry: &mut LocalTelemetry,
    ) -> Option<crate::simulator::ValidationRequest> {
        if let Some(i) = self.reqs.iter().position(|(_, pk)| *pk == builder_pubkey) {
            local_telemetry.sims_reqs_dropped += 1;
            if req.on_receive_ns() > self.reqs[i].0.on_receive_ns() {
                let evicted = std::mem::replace(&mut self.reqs[i].0, req);
                return Some(evicted);
            }
            return None;
        }
        self.reqs.push((req, builder_pubkey));
        local_telemetry.max_pending = local_telemetry.max_pending.max(self.reqs.len());
        None
    }

    fn next_req(&mut self) -> Option<crate::simulator::ValidationRequest> {
        let i =
            self.reqs.iter().enumerate().max_by_key(|(_, (r, _))| r.sort_key()).map(|(i, _)| i)?;
        Some(self.reqs.swap_remove(i).0)
    }

    /// Clear backlog of simulations from the previous bid slot.
    /// All pending requests are always for `last_bid_slot` (asserted on intake).
    fn clear(&mut self) {
        self.reqs.clear();
    }
}

/// Pending merged-block requests. There's exactly one merge builder connection, so unlike
/// `PendingRequests` (keyed per-builder) we only keep the last request per base block.
struct PendingMergeRequests {
    reqs: Vec<MergedValidationRequest>,
}

impl PendingMergeRequests {
    fn with_capacity(capacity: usize) -> Self {
        Self { reqs: Vec::with_capacity(capacity) }
    }

    /// Returns the evicted request if a newer one replaced it.
    fn store(&mut self, req: MergedValidationRequest) -> Option<MergedValidationRequest> {
        if let Some(i) = self.reqs.iter().position(|r| r.base_block_hash == req.base_block_hash) {
            if req.receive_ns > self.reqs[i].receive_ns {
                return Some(std::mem::replace(&mut self.reqs[i], req));
            }
            return None;
        }
        self.reqs.push(req);
        None
    }

    fn next_req(&mut self) -> Option<MergedValidationRequest> {
        let i = self.reqs.iter().enumerate().max_by_key(|(_, r)| r.receive_ns).map(|(i, _)| i)?;
        Some(self.reqs.swap_remove(i))
    }

    /// Clear backlog of simulations from the previous bid slot.
    fn clear(&mut self) {
        self.reqs.clear();
    }
}

fn infra_error(req: &ValidationRequest) -> SimulationResultInner {
    SimulationResultInner {
        submission_ref: req.submission_ref,
        optimistic_version: req.optimistic_version(),
        bid: None,
        result: Err(BlockSimError::RpcError),
    }
}

fn hydration_error(req: &ValidationRequest) -> SimulationResultInner {
    SimulationResultInner {
        submission_ref: req.submission_ref,
        optimistic_version: req.optimistic_version(),
        bid: None,
        result: Err(BlockSimError::RelayHydrationFailed),
    }
}

fn infra_merge_error(req: &MergedValidationRequest) -> MergedSimulationResultInner {
    MergedSimulationResultInner {
        merged_block_ix: req.merged_block_ix,
        result: Err(BlockSimError::RpcError),
    }
}

/// Converts a merged block into a synthetic `SignedBidSubmission` so it can be simulated
/// through the same SSZ/JSON dispatch the simulator already exposes for bid submissions.
/// `builder_pubkey`/`proposer_pubkey`/`signature` are zeroed: the simulator never checks the
/// BLS signature, and these fields are otherwise cosmetic (only `tx_sink` logging reads them).
fn merged_block_to_submission(
    response: &BlockMergeResponse,
    req: &MergedValidationRequest,
) -> Result<SignedBidSubmission, BlockSimError> {
    let payload = &response.execution_payload;
    let message = BidTrace {
        slot: req.slot,
        parent_hash: payload.parent_hash,
        block_hash: payload.block_hash,
        builder_pubkey: BlsPublicKeyBytes::default(),
        proposer_pubkey: BlsPublicKeyBytes::default(),
        proposer_fee_recipient: req.proposer_fee_recipient,
        gas_limit: payload.gas_limit,
        gas_used: payload.gas_used,
        value: response.proposer_value,
    };
    Ok(SignedBidSubmission {
        message,
        execution_payload: Arc::new(payload.clone()),
        blobs_bundle: Arc::new(response.blobs_bundle.clone()),
        execution_requests: Arc::new(response.execution_requests.clone()),
        signature: BlsSignatureBytes::default(),
    })
}

fn create_ssz_request(
    req: &ValidationRequest,
    submission: &SignedBidSubmission,
) -> SszValidationRequest {
    ssz_request(
        req.apply_blacklist,
        req.registered_gas_limit,
        req.parent_beacon_block_root,
        req.inclusion_list.clone(),
        submission,
    )
}

fn ssz_request(
    apply_blacklist: bool,
    registered_gas_limit: u64,
    parent_beacon_block_root: B256,
    inclusion_list: InclusionListWithMetadata,
    submission: &SignedBidSubmission,
) -> SszValidationRequest {
    SszValidationRequest {
        apply_blacklist,
        registered_gas_limit,
        parent_beacon_block_root,
        inclusion_list,
        decoder_params: None,
        signed_bid_submission: submission.as_ssz_bytes(),
    }
}

#[allow(clippy::too_many_arguments)]
fn ssz_merged_request(
    apply_blacklist: bool,
    registered_gas_limit: u64,
    parent_beacon_block_root: B256,
    inclusion_list: InclusionListWithMetadata,
    submission: &SignedBidSubmission,
    base_payment_tx_index: u64,
) -> SszMergedValidationRequest {
    SszMergedValidationRequest {
        apply_blacklist,
        registered_gas_limit,
        parent_beacon_block_root,
        inclusion_list,
        decoder_params: None,
        signed_bid_submission: submission.as_ssz_bytes(),
        base_payment_tx_index,
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use helix_common::decoder::{Encoding, SubmissionDecoderParams};
    use helix_tcp_types::{Compression, MergeType};
    use helix_types::{
        BlobWithMetadata, BlobsBundle, DehydratedBidSubmission, ExecutionPayload,
        ExecutionRequests, ForkName, MergedBlockTrace, SubmissionVersion, TestRandom,
        dehydrated_submission_with_txs_for_test, full_tx_for_test, tx_cache_key,
        tx_hash_ref_for_test,
    };
    use rand::{SeedableRng, rngs::SmallRng};

    use super::*;
    use crate::{SubmissionRef, auctioneer::SubmissionData};

    fn merge_response(
        payload: ExecutionPayload,
        proposer_value: U256,
        blobs: Vec<BlobWithMetadata>,
    ) -> BlockMergeResponse {
        let mut blobs_bundle = BlobsBundle::default();
        for blob in blobs {
            blobs_bundle.push_blob(blob.commitment, &blob.proofs, blob.blob, 9).unwrap();
        }
        BlockMergeResponse {
            base_block_hash: payload.parent_hash,
            execution_payload: payload,
            execution_requests: ExecutionRequests::default(),
            blobs_bundle,
            proposer_value,
            base_builder_revenue: U256::ZERO,
            relay_revenue: U256::ZERO,
            builder_inclusions: Default::default(),
            base_payment_tx_index: 0,
            trace: MergedBlockTrace::default(),
        }
    }

    fn test_tile() -> SimulatorTile {
        let (task_tx, rx) = crossbeam_channel::unbounded();
        SimulatorTile {
            simulators: vec![],
            ssz_sim_indices: vec![],
            requests: PendingRequests::with_capacity(4),
            priority_requests: PendingRequests::with_capacity(4),
            merge_requests: PendingMergeRequests::with_capacity(4),
            last_bid_slot: 0,
            local_telemetry: LocalTelemetry::default(),
            sim_slot_stats: vec![],
            task_tx,
            rx,
            sim_requests: Arc::new(SharedVector::default()),
            sim_results: Arc::new(SharedVector::default()),
            decoded: Arc::new(SharedVector::default()),
            merged_blocks: Arc::new(SharedVector::default()),
            hydration_cache: SimHydrationCache::new(),
            chain_info: ChainInfo::default(),
            accept_optimistic: Arc::new(AtomicBool::new(true)),
            failsafe_triggered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A tile with one `SimEntry` per spec: `(has_ssz, is_synced, pending)`.
    fn test_tile_with_sims(specs: &[(bool, bool, usize)]) -> SimulatorTile {
        let mut tile = test_tile();
        for &(has_ssz, is_synced, pending) in specs {
            let client = SimulatorClient::new(reqwest::Client::new(), SimulatorConfig {
                url: "http://localhost:8545".into(),
                namespace: "relay".into(),
                max_concurrent_tasks: 10,
                ssz_url: has_ssz.then(|| "http://localhost:8546".to_string()),
            });
            let mut entry = SimEntry::new(client);
            entry.is_synced = is_synced;
            entry.pending = pending;
            tile.simulators.push(entry);
        }
        tile.ssz_sim_indices = tile
            .simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.client.ssz_url.is_some())
            .map(|(i, _)| i)
            .collect();
        tile.sim_slot_stats = vec![SimSlotStats::default(); tile.simulators.len()];
        tile
    }

    /// A decoded submission that carries only a hash reference, so hydration
    /// against an empty cache must miss.
    fn unhydratable_submission(builder: BlsPublicKeyBytes) -> SubmissionDataWithSpan {
        let tx = full_tx_for_test(1);
        let mut dehydrated =
            dehydrated_submission_with_txs_for_test(vec![tx_hash_ref_for_test(tx_cache_key(&tx))]);
        dehydrated.set_builder_pubkey_for_test(builder);
        decoded_from(dehydrated)
    }

    fn decoded_from(dehydrated: DehydratedBidSubmission) -> SubmissionDataWithSpan {
        let submission_data = SubmissionData {
            submission_ref: SubmissionRef::Internal,
            submission: Submission::Dehydrated(dehydrated),
            merging_data: None,
            bid_adjustment_data: None,
            version: SubmissionVersion::new(0, None),
            withdrawals_root: B256::ZERO,
            trace: SubmissionTrace::default(),
            decoder_params: SubmissionDecoderParams {
                compression: Compression::None,
                encoding: Encoding::Ssz,
                merge_type: MergeType::default(),
                is_dehydrated: true,
                dehydrated_v2: false,
                with_mergeable_data: false,
                with_adjustments: false,
                mark_all_txs_mergeable: false,
                fork_name: ForkName::Deneb,
            },
            is_pessimistic: false,
        };
        SubmissionDataWithSpan { submission_data, span: tracing::Span::none(), sent_at: Nanos(0) }
    }

    fn validation_request(decoded_ix: usize) -> ValidationRequest {
        ValidationRequest {
            is_top_bid: false,
            is_optimistic: false,
            apply_blacklist: false,
            registered_gas_limit: 30_000_000,
            parent_beacon_block_root: B256::ZERO,
            inclusion_list: InclusionListWithMetadata::default(),
            decoded_ix,
            receive_ns: 0,
            submission_ref: SubmissionRef::Internal,
        }
    }

    /// gattaca-com/helix#563: a hydration miss must be attributable to a builder
    /// without grepping, so the per-slot stats name who is failing.
    #[test]
    fn hydration_failures_are_counted_per_builder() {
        let mut tile = test_tile_with_sims(&[(true, true, 0)]);
        let alice = BlsPublicKeyBytes::from([1u8; 48]);
        let bob = BlsPublicKeyBytes::from([2u8; 48]);

        for builder in [alice, alice, bob] {
            let ix = tile.decoded.push(unhydratable_submission(builder));
            tile.spawn_sim(0, validation_request(ix));
        }

        assert_eq!(tile.local_telemetry.hydration_failures.get(&alice), Some(&2));
        assert_eq!(tile.local_telemetry.hydration_failures.get(&bob), Some(&1));
    }

    /// The counter is per slot, so `report` must leave it empty for the next one.
    #[test]
    fn hydration_failure_counts_reset_each_slot() {
        let mut tile = test_tile_with_sims(&[(true, true, 0)]);
        let ix = tile.decoded.push(unhydratable_submission(BlsPublicKeyBytes::from([3u8; 48])));
        tile.spawn_sim(0, validation_request(ix));
        assert_eq!(tile.local_telemetry.hydration_failures.len(), 1);

        tile.report();

        assert!(tile.local_telemetry.hydration_failures.is_empty());
    }

    /// gattaca-com/helix#551: selection follows pending count, not a pubkey hash.
    #[test]
    fn select_simulator_picks_the_least_pending_ssz_sim() {
        let tile = test_tile_with_sims(&[(false, true, 0), (true, true, 5), (true, true, 1)]);

        assert_eq!(tile.select_simulator(), Some(2));
    }

    /// SSZ is still preferred over JSON-RPC even when a JSON sim is idler.
    #[test]
    fn select_simulator_prefers_ssz_over_a_less_loaded_json_sim() {
        let tile = test_tile_with_sims(&[(false, true, 0), (true, true, 4)]);

        assert_eq!(tile.select_simulator(), Some(1));
    }

    #[test]
    fn select_simulator_falls_back_to_a_json_sim_when_no_ssz_sim_is_available() {
        let tile = test_tile_with_sims(&[(false, true, 3), (true, false, 0)]);

        assert_eq!(tile.select_simulator(), Some(0));
    }

    #[test]
    fn select_simulator_skips_saturated_sims() {
        // max_concurrent_tasks is 10, so the first SSZ sim cannot take more work.
        let tile = test_tile_with_sims(&[(true, true, 10), (true, true, 9)]);

        assert_eq!(tile.select_simulator(), Some(1));
    }

    #[test]
    fn select_simulator_skips_sims_with_an_open_breaker() {
        let mut tile = test_tile_with_sims(&[(true, true, 0), (true, true, 8)]);
        let now = Instant::now();
        open_breaker(&mut tile.simulators[0], now);

        assert_eq!(tile.select_simulator_at(now), Some(1));
    }

    fn open_breaker(sim: &mut SimEntry, now: Instant) {
        for _ in 0..SIM_FAULTS_TO_OPEN {
            sim.record_failure(&BlockSimError::RpcError, now);
        }
    }

    fn fast_sim(max_concurrent_tasks: usize) -> SimEntry {
        SimEntry::new(SimulatorClient::new(reqwest::Client::new(), SimulatorConfig {
            url: "http://localhost:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks,
            ssz_url: Some("http://localhost:8546".into()),
        }))
    }

    /// The configured cap is a ceiling, never the operating point.
    #[test]
    fn in_flight_never_passes_the_current_limit() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        sim.limit = 3;

        sim.pending = 2;
        assert!(sim.can_simulate_at(now));

        sim.pending = 3;
        assert!(!sim.can_simulate_at(now), "the limit must bind before the configured cap");
    }

    #[test]
    fn a_fast_success_raises_the_limit_up_to_the_ceiling() {
        let mut sim = fast_sim(4);
        sim.limit = 1;

        for _ in 0..10 {
            sim.record_success(LATENCY_TARGET / 2);
        }

        assert_eq!(sim.limit, 4, "the limit must stop at the configured cap");
    }

    #[test]
    fn a_slow_success_halves_the_limit() {
        let mut sim = fast_sim(32);
        sim.limit = 32;

        sim.record_success(LATENCY_TARGET * 2);

        assert_eq!(sim.limit, 16);
    }

    #[test]
    fn the_limit_never_falls_below_one() {
        let mut sim = fast_sim(32);
        sim.limit = 32;

        for _ in 0..20 {
            sim.record_success(LATENCY_TARGET * 2);
        }

        assert_eq!(sim.limit, 1, "a sim must keep one slot to prove it recovered");
    }

    /// Today one error removes a sim for five slots. It must take a run of them.
    #[test]
    fn one_sim_fault_does_not_remove_a_sim() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        sim.record_failure(&BlockSimError::RpcError, now);

        assert!(sim.can_simulate_at(now));
    }

    #[test]
    fn a_run_of_sim_faults_opens_the_breaker() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        open_breaker(&mut sim, now);

        assert!(!sim.can_simulate_at(now));
        assert_eq!(sim.effective_limit(now), 0);
    }

    #[test]
    fn a_success_clears_the_fault_run() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        for _ in 0..SIM_FAULTS_TO_OPEN - 1 {
            sim.record_failure(&BlockSimError::RpcError, now);
        }
        sim.record_success(LATENCY_TARGET / 2);
        sim.record_failure(&BlockSimError::RpcError, now);

        assert!(sim.can_simulate_at(now), "the run must restart after a success");
    }

    #[test]
    fn an_expired_breaker_admits_exactly_one_probe() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        open_breaker(&mut sim, now);
        let probe_time = now + BREAKER_BACKOFF_START;

        assert_eq!(sim.effective_limit(probe_time), 1);

        sim.pending = 1;
        assert!(!sim.can_simulate_at(probe_time), "a second probe must wait");
    }

    #[test]
    fn a_successful_probe_restores_full_service() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        sim.limit = 8;
        open_breaker(&mut sim, now);
        let probe_time = now + BREAKER_BACKOFF_START;

        sim.record_success(LATENCY_TARGET / 2);

        assert!(sim.effective_limit(probe_time) > 1, "a healthy sim must return at once");
        sim.pending = 1;
        assert!(sim.can_simulate_at(probe_time));
    }

    #[test]
    fn a_failed_probe_extends_the_backoff() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        open_breaker(&mut sim, now);
        let probe_time = now + BREAKER_BACKOFF_START;

        sim.record_failure(&BlockSimError::RpcError, probe_time);

        assert_eq!(sim.effective_limit(probe_time), 0, "the breaker must close again");
        assert_eq!(sim.effective_limit(probe_time + BREAKER_BACKOFF_START), 0);
        assert_eq!(sim.effective_limit(probe_time + BREAKER_BACKOFF_START * 2), 1);
    }

    #[test]
    fn the_backoff_is_capped() {
        let mut now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        open_breaker(&mut sim, now);

        for _ in 0..10 {
            now += BREAKER_BACKOFF_MAX;
            sim.record_failure(&BlockSimError::RpcError, now);
        }

        assert_eq!(sim.effective_limit(now + BREAKER_BACKOFF_MAX), 1);
    }

    /// The parent is simply newer than the node's head. Every sim sees this at a
    /// slot boundary, so it must never take a sim out of service.
    #[test]
    fn a_missing_parent_arms_no_breaker() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;
        let err = BlockSimError::BlockValidationFailed("parent block not found".into());

        for _ in 0..SIM_FAULTS_TO_OPEN * 3 {
            sim.record_failure(&err, now);
        }

        assert!(sim.can_simulate_at(now));
    }

    /// A sim with a small limit is not as free as a sim with a big one at the same
    /// pending count, so selection must compare the share used, not the raw count.
    #[test]
    fn select_simulator_picks_the_lowest_utilization() {
        let now = Instant::now();
        let mut tile = test_tile_with_sims(&[(true, true, 8), (true, true, 2)]);
        tile.simulators[0].limit = 32;
        tile.simulators[1].limit = 4;

        assert_eq!(tile.select_simulator_at(now), Some(0));
    }

    /// Open and probe share one enum variant, so the report must still tell them apart.
    #[test]
    fn the_health_label_names_the_probe_window() {
        let now = Instant::now();
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        assert_eq!(sim.health(now), SimHealth::Ok);

        open_breaker(&mut sim, now);
        assert_eq!(sim.health(now), SimHealth::Open);
        assert_eq!(sim.health(now + BREAKER_BACKOFF_START), SimHealth::Probe);
    }

    #[test]
    fn an_unsynced_sim_reports_unsynced() {
        let now = Instant::now();
        let sim = fast_sim(8);

        assert_eq!(sim.health(now), SimHealth::Unsynced);
    }

    #[test]
    fn one_failed_sync_poll_does_not_remove_a_sim() {
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        sim.record_sync(None);

        assert!(sim.is_synced, "a single dropped poll must not remove a sim");
    }

    #[test]
    fn a_run_of_failed_sync_polls_removes_a_sim() {
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        for _ in 0..SYNC_FAILURES_TO_UNSYNC {
            sim.record_sync(None);
        }

        assert!(!sim.is_synced);

        sim.record_sync(Some(true));
        assert!(sim.is_synced, "one good poll must restore a sim");
    }

    /// An honest "I am syncing" is not a dropped poll. Trust it at once.
    #[test]
    fn a_node_that_reports_syncing_is_removed_at_once() {
        let mut sim = fast_sim(8);
        sim.is_synced = true;

        sim.record_sync(Some(false));

        assert!(!sim.is_synced);
    }

    #[test]
    fn select_simulator_none_when_nothing_can_simulate() {
        let tile = test_tile_with_sims(&[(true, false, 0), (false, false, 0)]);

        assert_eq!(tile.select_simulator(), None);
    }

    /// gattaca-com/helix#537: a submission the tile queues instead of simulating
    /// must still put its full transactions in the cache, so a later submission
    /// that references them can be hydrated whatever order the two arrive in.
    #[test]
    fn feed_cache_fills_from_a_submission_that_is_not_simulated() {
        let tx = full_tx_for_test(1);
        let earlier = dehydrated_submission_with_txs_for_test(vec![tx.clone()]);
        let later =
            dehydrated_submission_with_txs_for_test(vec![tx_hash_ref_for_test(tx_cache_key(&tx))]);

        let mut tile = test_tile();
        tile.feed_cache(&Submission::Dehydrated(earlier));

        let max_blobs = tile.chain_info.max_blobs_per_block();
        assert!(
            tile.hydration_cache.can_hydrate(&later, max_blobs),
            "a queued submission's transactions must be usable by the next submission"
        );
    }

    /// gattaca-com/helix#537: the auctioneer rejects a submission in validation, feeds its
    /// own cache and asks for no simulation. The sim tile must still learn that
    /// submission's transactions, or every later reference to them misses.
    #[test]
    fn a_feed_cache_message_fills_from_a_submission_that_is_never_simulated() {
        let tx = full_tx_for_test(1);
        let earlier = dehydrated_submission_with_txs_for_test(vec![tx.clone()]);
        let later =
            dehydrated_submission_with_txs_for_test(vec![tx_hash_ref_for_test(tx_cache_key(&tx))]);
        let decoded = decoded_from(earlier);
        let bid_slot = decoded.submission_data.submission.bid_slot();

        let mut tile = test_tile();
        tile.last_bid_slot = bid_slot;
        let ix = tile.decoded.push(decoded);
        tile.handle_feed_cache(ix, bid_slot);

        assert!(
            tile.hydration_cache.can_hydrate(&later, tile.chain_info.max_blobs_per_block()),
            "a rejected submission must still fill the sim tile's cache"
        );
    }

    /// The cache is cleared on each slot, so a feed from another slot must not refill it.
    #[test]
    fn a_feed_cache_message_for_another_slot_is_ignored() {
        let tx = full_tx_for_test(1);
        let decoded = decoded_from(dehydrated_submission_with_txs_for_test(vec![tx]));
        let bid_slot = decoded.submission_data.submission.bid_slot();

        let mut tile = test_tile();
        tile.last_bid_slot = bid_slot + 1;
        let ix = tile.decoded.push(decoded);
        tile.handle_feed_cache(ix, bid_slot);

        assert_eq!(tile.hydration_cache.tx_count(), 0);
    }

    /// A full submission carries no hash references, so feeding it is a no-op for
    /// the cache: only dehydrated submissions contribute keys.
    #[test]
    fn feed_cache_ignores_full_submissions() {
        let mut tile = test_tile();
        let before = tile.hydration_cache.tx_count();

        let mut rng = SmallRng::seed_from_u64(7);
        let payload = ExecutionPayload::random_for_test(&mut rng);
        let response = merge_response(payload, U256::ZERO, vec![]);
        let req = merge_request(response.base_block_hash, 0);
        let full = merged_block_to_submission(&response, &req).unwrap();

        tile.feed_cache(&Submission::Full(full));

        assert_eq!(tile.hydration_cache.tx_count(), before);
    }

    fn merge_request(base_block_hash: B256, receive_ns: u64) -> MergedValidationRequest {
        MergedValidationRequest {
            merged_block_ix: 0,
            base_block_hash,
            slot: 123,
            parent_beacon_block_root: B256::repeat_byte(9),
            proposer_fee_recipient: Address::repeat_byte(7),
            registered_gas_limit: 30_000_000,
            apply_blacklist: true,
            inclusion_list: InclusionListWithMetadata::default(),
            receive_ns,
        }
    }

    #[test]
    fn merged_block_to_submission_derives_bid_trace_from_payload_and_context() {
        let mut rng = SmallRng::seed_from_u64(1);
        let payload = ExecutionPayload::random_for_test(&mut rng);
        let response = merge_response(payload.clone(), U256::from(42u64), vec![]);
        let req = merge_request(response.base_block_hash, 0);

        let submission = merged_block_to_submission(&response, &req).unwrap();

        assert_eq!(submission.message.slot, req.slot);
        assert_eq!(submission.message.parent_hash, payload.parent_hash);
        assert_eq!(submission.message.block_hash, payload.block_hash);
        assert_eq!(submission.message.gas_limit, payload.gas_limit);
        assert_eq!(submission.message.gas_used, payload.gas_used);
        assert_eq!(submission.message.value, response.proposer_value);
        assert_eq!(submission.message.proposer_fee_recipient, req.proposer_fee_recipient);
        assert_eq!(submission.message.builder_pubkey, BlsPublicKeyBytes::default());
        assert_eq!(submission.message.proposer_pubkey, BlsPublicKeyBytes::default());
        assert_eq!(submission.signature, BlsSignatureBytes::default());
    }

    #[test]
    fn merged_block_to_submission_converts_appended_blobs_to_blobs_bundle() {
        let mut rng = SmallRng::seed_from_u64(2);
        let payload = ExecutionPayload::random_for_test(&mut rng);
        let blob = BlobWithMetadata {
            commitment: Default::default(),
            proofs: vec![Default::default(); 128],
            blob: Default::default(),
        };
        let response = merge_response(payload, U256::ZERO, vec![blob.clone()]);
        let req = merge_request(response.base_block_hash, 0);

        let submission = merged_block_to_submission(&response, &req).unwrap();

        assert_eq!(submission.blobs_bundle.commitments.len(), 1);
        assert_eq!(submission.blobs_bundle.commitments[0], blob.commitment);
        assert_eq!(submission.blobs_bundle.proofs, blob.proofs);
        assert_eq!(submission.blobs_bundle.blobs.len(), 1);
    }

    #[test]
    fn pending_merge_requests_evicts_older_same_base_block() {
        let mut pending = PendingMergeRequests::with_capacity(4);
        let base = B256::repeat_byte(1);
        assert!(pending.store(merge_request(base, 10)).is_none());

        let evicted = pending.store(merge_request(base, 20));
        assert_eq!(evicted.map(|r| r.receive_ns), Some(10));
    }

    #[test]
    fn pending_merge_requests_keeps_existing_if_new_is_older() {
        let mut pending = PendingMergeRequests::with_capacity(4);
        let base = B256::repeat_byte(1);
        pending.store(merge_request(base, 20));

        let evicted = pending.store(merge_request(base, 10));
        assert!(evicted.is_none());
        assert_eq!(pending.next_req().map(|r| r.receive_ns), Some(20));
    }

    #[test]
    fn pending_merge_requests_next_req_returns_and_removes() {
        let mut pending = PendingMergeRequests::with_capacity(4);
        pending.store(merge_request(B256::repeat_byte(1), 5));
        pending.store(merge_request(B256::repeat_byte(2), 15));

        let next = pending.next_req().unwrap();
        assert_eq!(next.receive_ns, 15);
        assert_eq!(pending.next_req().unwrap().receive_ns, 5);
        assert!(pending.next_req().is_none());
    }

    #[test]
    fn pending_merge_requests_clear_empties_queue() {
        let mut pending = PendingMergeRequests::with_capacity(4);
        pending.store(merge_request(B256::repeat_byte(1), 5));
        pending.clear();
        assert!(pending.next_req().is_none());
    }
}
