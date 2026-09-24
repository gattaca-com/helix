use std::{
    self,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use flux::timing::Nanos;
use flux_profiler::timed;
use helix_common::{
    SimulatorConfig, SubmissionTrace,
    api::builder_api::InclusionListWithMetadata,
    bid_submission::OptimisticVersion,
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
    BidTrace, BlsPublicKeyBytes, BlsSignatureBytes, SignedBidSubmission, SubmissionVersion,
};
use rustc_hash::FxHashMap;
use ssz::Encode as _;
use tracing::{debug, error, info, warn};

use crate::{
    ValidationRequest,
    auctioneer::Bid,
    simulator::{
        BlockMergeResponse, MergedValidationRequest, SimPriority, SimResult,
        client::SimulatorClient,
    },
};

pub struct SimPool {
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
    /// Internal channel: async tasks notify the pool when work completes.
    task_tx: crossbeam_channel::Sender<SimPoolEvent>,
    rx: crossbeam_channel::Receiver<SimPoolEvent>,
    /// Sampled-so-far and seen-so-far counts per builder for the current slot.
    sample_state: FxHashMap<BlsPublicKeyBytes, SampleState>,
    /// If we have any synced simulator
    accept_optimistic: bool,
    /// Results produced synchronously (dropped/sampled-out/drained requests,
    /// no-fork-method failures), drained by `poll`.
    answered: Vec<SimDone>,
}

pub struct SimDone {
    pub result: SimResult,
    pub elapsed: Duration,
}

impl SimPool {
    pub fn new(configs: Vec<SimulatorConfig>) -> Self {
        let (task_tx, rx) = crossbeam_channel::unbounded();

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
                            if sync_tx.send(SimPoolEvent::SyncStatus { id, reported }).is_err() {
                                error!("failed to send sync status to sim pool");
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

        let ssz_sim_indices: Vec<usize> = simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.client.ssz_url.is_some())
            .map(|(i, _)| i)
            .collect();

        let sim_slot_stats = vec![SimSlotStats::default(); simulators.len()];

        Self {
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
            sample_state: FxHashMap::default(),
            accept_optimistic: true,
            answered: Vec::new(),
        }
    }

    pub fn poll(&mut self) -> Vec<SimDone> {
        let mut done = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                SimPoolEvent::TaskDone { id, error, result, elapsed } => {
                    self.on_task_response(id, error, elapsed);
                    done.push(SimDone {
                        result: *result,
                        elapsed: elapsed.unwrap_or(Duration::ZERO),
                    });
                }
                SimPoolEvent::SyncStatus { id, reported } => {
                    self.handle_sync_status(id, reported);
                }
            }
        }
        done.append(&mut self.answered);
        done
    }

    pub fn on_new_slot(&mut self, bid_slot: u64) {
        if self.last_bid_slot > 0 {
            self.report();
        }

        self.last_bid_slot = bid_slot;
        let left = [self.requests.drain(), self.priority_requests.drain()].concat();
        for req in left {
            self.answer_dropped(&req);
        }
        self.merge_requests.clear();
        self.sample_state.clear();
    }

    pub fn accept_optimistic(&self) -> bool {
        self.accept_optimistic
    }

    fn handle_sync_status(&mut self, id: usize, reported: Option<bool>) {
        self.simulators[id].record_sync(reported);
        self.refresh_accept_optimistic(Instant::now());
    }

    fn refresh_accept_optimistic(&mut self, now: Instant) {
        let new = self.simulators.iter().any(|s| s.can_simulate_light(now));
        let prev = std::mem::replace(&mut self.accept_optimistic, new);
        if new != prev {
            let synced = self.simulators.iter().filter(|s| s.is_synced).count();
            let answering = self.simulators.iter().filter(|s| s.answered_recently(now)).count();
            warn!(
                prev,
                new,
                synced,
                answering,
                sims = self.simulators.len(),
                "changing accept_optimistic simulation status"
            );
        }
    }

    /// Whether this builder's next sampled bid should run. Every builder gets
    /// `SAMPLE_FLOOR` simulations a slot however little it submits, then one in
    /// `SAMPLE_EVERY` after that.
    fn take_sample(&mut self, builder_pubkey: BlsPublicKeyBytes) -> bool {
        let state = self.sample_state.entry(builder_pubkey).or_default();
        state.seen += 1;
        if state.sampled < SAMPLE_FLOOR || state.seen.is_multiple_of(SAMPLE_EVERY) {
            state.sampled += 1;
            return true;
        }
        false
    }

    /// Answers a request no simulator will ever run, so the builder never waits on a
    /// result that is not coming. A dropped simulation never demotes. An optimistic
    /// submission was answered when it was sorted, so only the other ones need this.
    fn answer_dropped(&mut self, req: &crate::simulator::ValidationRequest) {
        if req.is_optimistic {
            return;
        }

        self.answered.push(SimDone {
            result: SimResult::Validate((
                0,
                Some(SimulationResultInner {
                    submission_ref: req.submission_ref,
                    optimistic_version: req.optimistic_version(),
                    bid: None,
                    result: Err(BlockSimError::SimulationDropped),
                }),
            )),
            elapsed: Duration::ZERO,
        });
    }

    #[timed]
    pub fn dispatch(&mut self, req: crate::simulator::ValidationRequest, fast_track: bool) {
        let builder_pubkey = req.submission.message.builder_pubkey;
        let queue_key =
            QueueKey { parent_hash: req.submission.message.parent_hash, builder_pubkey };
        let version = req.version;
        debug_assert_eq!(req.submission.message.slot, self.last_bid_slot);
        if req.submission.message.slot != self.last_bid_slot {
            error!(
                slot = req.submission.message.slot,
                last_bid_slot = self.last_bid_slot,
                "sim request for unexpected slot"
            );
        }

        self.local_telemetry.sims_reqs += 1;

        if req.priority == SimPriority::Sample && !self.take_sample(builder_pubkey) {
            self.local_telemetry.sample_skipped += 1;
            self.answer_dropped(&req);
            return;
        }

        let sim_id = self.select_simulator();

        if let Some(id) = sim_id {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_sim(id, req)
        } else {
            self.local_telemetry.queued += 1;
            let dropped = if fast_track {
                self.priority_requests.store(req, queue_key, version, &mut self.local_telemetry)
            } else {
                self.requests.store(req, queue_key, version, &mut self.local_telemetry)
            };
            if let Some(dropped) = dropped {
                self.answer_dropped(&dropped);
            }
        }
    }

    #[timed]
    pub fn dispatch_merged(
        &mut self,
        req: MergedValidationRequest,
        response: Arc<BlockMergeResponse>,
    ) {
        self.local_telemetry.sims_reqs += 1;

        if let Some(id) = self.next_client(Instant::now()) {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_merge_sim(id, req, response);
        } else {
            self.local_telemetry.queued += 1;
            self.merge_requests.store(req, response);
        }
    }

    fn on_task_response(
        &mut self,
        id: usize,
        error: Option<BlockSimError>,
        elapsed: Option<Duration>,
    ) {
        let now = Instant::now();
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);

        match error {
            Some(err) => sim.record_failure(&err, now),
            None => {
                if let Some(elapsed) = elapsed {
                    sim.record_success(elapsed, now);
                }
            }
        }

        if let Some(elapsed) = elapsed {
            let stats = &mut self.sim_slot_stats[id];
            stats.count += 1;
            stats.total_time += elapsed;
        }

        self.refresh_accept_optimistic(now);

        if let Some(id) = self.next_client(now) {
            if let Some(req) = self.priority_requests.next_req().or(self.requests.next_req()) {
                self.local_telemetry.sims_sent_from_queue += 1;
                self.spawn_sim(id, req);
            } else if let Some((req, response)) = self.merge_requests.next_req() {
                self.spawn_merge_sim(id, req, response);
            }
        }
    }

    #[timed]
    fn spawn_sim(&mut self, id: usize, req: ValidationRequest) {
        let submission = req.submission.clone();
        let tx_root = req.tx_root;
        let version = req.version;
        let trace = req.trace;
        let submission_ref = req.submission_ref;

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
                self.answered.push(SimDone {
                    result: SimResult::Validate((
                        id,
                        Some(SimulationResultInner {
                            submission_ref: req.submission_ref,
                            optimistic_version: req.optimistic_version(),
                            bid: None,
                            result: Err(BlockSimError::UnsupportedFork(fork)),
                        }),
                    )),
                    elapsed: Duration::ZERO,
                });
                return;
            };
            SimDispatch::Json { to_send: builder, method: method.to_owned() }
        };
        sim.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(sim.pending);
        let timer = SimulatorMetrics::timer(sim.client.endpoint());
        let task_tx = self.task_tx.clone();
        spawn_tracked!(async move {
            let start_sim = Nanos::now();
            let block_hash = submission.block_hash();
            debug!(%block_hash, "sending simulation request");

            let optimistic_version = req.optimistic_version();
            SimulatorMetrics::sim_count(optimistic_version.is_optimistic());
            let (mut res, ssz_retry) = match dispatch {
                SimDispatch::Ssz { to_send, ssz_url, http } => {
                    let request = create_ssz_request(&req, &submission);
                    let res = SimulatorClient::do_sim_request(
                        &request,
                        req.is_top_bid,
                        to_send,
                        &ssz_url,
                    )
                    .await;
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
                    res = SimulatorClient::do_sim_request(
                        &retry_req,
                        req.is_top_bid,
                        to_send,
                        &ssz_url,
                    )
                    .await;
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
            SimulatorMetrics::sim_builder_outcome(
                &bid.builder_pubkey.to_string(),
                req.priority.label(),
                sim_outcome(error.as_ref()),
            );
            let inner = SimulationResultInner {
                submission_ref,
                result: res.map(|()| trace),
                bid: Some(bid),
                optimistic_version,
            };

            let _ = task_tx.send(SimPoolEvent::TaskDone {
                id,
                error,
                result: Box::new(SimResult::Validate((id, Some(inner)))),
                elapsed: Some(Duration::from_secs_f64(time)),
            });
        });
    }

    #[timed]
    fn spawn_merge_sim(
        &mut self,
        id: usize,
        req: MergedValidationRequest,
        response: Arc<BlockMergeResponse>,
    ) {
        let base_payment_tx_index = response.base_payment_tx_index as u64;
        let block_hash = response.execution_payload.block_hash;

        let sim = &mut self.simulators[id];
        let dispatch = if let Some(url) = &sim.client.ssz_url {
            MergedSimDispatch::Ssz(
                sim.client.client.post(format!("{url}/validate_merged")),
                url.clone(),
            )
        } else {
            let (builder, method) = sim.client.merged_sim_request_builder();
            MergedSimDispatch::Json { to_send: builder, method: method.to_owned() }
        };
        sim.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(sim.pending);
        let timer = SimulatorMetrics::timer(sim.client.endpoint());
        let task_tx = self.task_tx.clone();
        let apply_blacklist = req.apply_blacklist;
        let registered_gas_limit = req.registered_gas_limit;
        let parent_beacon_block_root = req.parent_beacon_block_root;
        spawn_tracked!(async move {
            let start_sim = Nanos::now();
            let inclusion_list = req.inclusion_list.clone();
            let submission = match merged_block_to_submission(&response, &req) {
                Ok(submission) => submission,
                Err(err) => {
                    let inner = MergedSimulationResultInner { block_hash, result: Err(err) };
                    let _ = task_tx.send(SimPoolEvent::TaskDone {
                        id,
                        error: None,
                        result: Box::new(SimResult::ValidateMerged((id, Some(inner)))),
                        elapsed: None,
                    });
                    return;
                }
            };
            let block_hash = submission.execution_payload.block_hash;
            debug!(%block_hash, "sending merged block simulation request");

            SimulatorMetrics::sim_count(false);
            let res = match dispatch {
                MergedSimDispatch::Ssz(to_send, endpoint) => {
                    let request = ssz_merged_request(
                        apply_blacklist,
                        registered_gas_limit,
                        parent_beacon_block_root,
                        inclusion_list,
                        &submission,
                        base_payment_tx_index,
                    );
                    SimulatorClient::do_sim_request(&request, false, to_send, &endpoint).await
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
            let inner = MergedSimulationResultInner { block_hash, result: res };
            let _ = task_tx.send(SimPoolEvent::TaskDone {
                id,
                error,
                result: Box::new(SimResult::ValidateMerged((id, Some(inner)))),
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

    fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);
        let queue_left = self.requests.reqs.len() + self.priority_requests.reqs.len();

        SimulatorMetrics::sim_mananger_count("sims_sent_immediately", tel.sims_sent_immediately);
        SimulatorMetrics::sim_mananger_count("sims_reqs_dropped", tel.sims_reqs_dropped);
        SimulatorMetrics::sim_mananger_count("sample_skipped", tel.sample_skipped);
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

        info!(
            bid_slot = self.last_bid_slot,
            sims_reqs = tel.sims_reqs,
            sims_sent_immediately = tel.sims_sent_immediately,
            queued = tel.queued,
            sims_sent_from_queue = tel.sims_sent_from_queue,
            sims_reqs_dropped = tel.sims_reqs_dropped,
            sample_skipped = tel.sample_skipped,
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
    /// When this node last answered a simulation. `None` until it answers one.
    last_success: Option<Instant>,
    /// Adaptive concurrency limit, bounded by `max_concurrent_tasks`.
    limit: usize,
    /// Consecutive sim faults, reset by any answer from the node.
    faults: usize,
    /// Consecutive failed sync polls, reset by any answer from the node.
    sync_failures: usize,
    last_decrease: Option<Instant>,
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
            last_success: None,
            limit,
            faults: 0,
            sync_failures: 0,
            last_decrease: None,
            pending: 0,
        }
    }

    fn ceiling(&self) -> usize {
        self.client.config.max_concurrent_tasks.max(MIN_LIMIT)
    }

    fn floor(&self) -> usize {
        DECREASE_FLOOR.min(self.ceiling())
    }

    fn decrease(&mut self, now: Instant) {
        if self.last_decrease.is_some_and(|at| now.saturating_duration_since(at) < DECREASE_WINDOW)
        {
            return;
        }

        self.last_decrease = Some(now);
        self.limit = (self.limit * 4 / 5).min(self.limit.saturating_sub(1)).max(self.floor());
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

    /// A lighter check to decide whether we should accept optimistic submissions. A breaker
    /// backoff is not a loss of capability, so it only closes this once the node has gone
    /// `OPTIMISTIC_GRACE` without answering a simulation.
    fn can_simulate_light(&self, now: Instant) -> bool {
        self.is_synced && (matches!(self.breaker, Breaker::Closed) || self.answered_recently(now))
    }

    fn answered_recently(&self, now: Instant) -> bool {
        self.last_success.is_some_and(|at| now.saturating_duration_since(at) < OPTIMISTIC_GRACE)
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
    fn record_success(&mut self, elapsed: Duration, now: Instant) {
        self.faults = 0;
        self.breaker = Breaker::Closed;
        self.last_success = Some(now);
        if elapsed <= LATENCY_TARGET {
            self.limit = (self.limit + 1).min(self.ceiling());
        } else {
            self.decrease(now);
        }
    }

    fn record_failure(&mut self, err: &BlockSimError, now: Instant) {
        if !err.is_sim_fault() {
            return;
        }

        self.faults += 1;
        self.decrease(now);

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
const LATENCY_TARGET: Duration = Duration::from_millis(500);
const MIN_LIMIT: usize = 1;
const DECREASE_FLOOR: usize = 4;
const DECREASE_WINDOW: Duration = Duration::from_millis(250);
const UTILIZATION_SCALE: usize = 1_000;
const SIM_FAULTS_TO_OPEN: usize = 3;
/// How long every simulator must go without answering a simulation before the relay stops
/// accepting optimistic submissions. Longer than `BREAKER_BACKOFF_START`, shorter than
/// `BREAKER_BACKOFF_MAX`, so a backoff is ridden out but a real outage is not.
const OPTIMISTIC_GRACE: Duration = Duration::from_secs(30);
/// Simulations every builder gets each slot, however few bids it sends.
const SAMPLE_FLOOR: u32 = 3;
/// After the floor, one bid in this many joins the sample.
const SAMPLE_EVERY: u32 = 64;
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
    /// Optimistic bids that could not win and were not drawn into their builder's
    /// validity sample, so no simulator ran them.
    sample_skipped: usize,
    /// Requests with no simulator free at intake, queued for later dispatch.
    /// `sims_reqs == sims_sent_immediately + queued`.
    queued: usize,
    /// Queued requests dispatched once a simulator freed up (queued ->
    /// sims_sent_from_queue, or evicted by a fresher request for the same
    /// builder -> sims_reqs_dropped, or still resident at slot end ->
    /// `queue_left`, read live from the queues rather than stored here).
    sims_sent_from_queue: usize,
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
    pub block_hash: B256,
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
    Ssz(reqwest::RequestBuilder, String),
    Json { to_send: reqwest::RequestBuilder, method: String },
}

/// Internal-only events: async task → pool (not tile-to-tile).
enum SimPoolEvent {
    /// `elapsed` is `None` for infra errors where no request was actually sent
    /// (e.g. unsupported fork).
    TaskDone {
        id: usize,
        error: Option<BlockSimError>,
        result: Box<SimResult>,
        elapsed: Option<Duration>,
    },
    SyncStatus {
        id: usize,
        reported: Option<bool>,
    },
}

/// Per-builder sampling counters for the current slot.
#[derive(Default, Clone, Copy)]
struct SampleState {
    seen: u32,
    sampled: u32,
}

#[derive(Default, Clone, Copy)]
struct SimSlotStats {
    count: u32,
    total_time: Duration,
}

/// Identifies the one queued request a builder may hold on a fork. Mirrors the bid sorter,
/// which keys on parent hash then builder pubkey.
#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueKey {
    parent_hash: B256,
    builder_pubkey: BlsPublicKeyBytes,
}

/// Pending requests, we only keep the last one for each builder on each fork.
struct PendingRequests {
    reqs: Vec<(crate::simulator::ValidationRequest, QueueKey, SubmissionVersion)>,
}

impl PendingRequests {
    fn with_capacity(capacity: usize) -> Self {
        Self { reqs: Vec::with_capacity(capacity) }
    }

    /// Returns the request this store dropped: the one it replaced, or `req` itself when
    /// the queue already holds a fresher one. Either way that request is never simulated,
    /// so the caller must answer it.
    fn store(
        &mut self,
        req: crate::simulator::ValidationRequest,
        key: QueueKey,
        version: SubmissionVersion,
        local_telemetry: &mut LocalTelemetry,
    ) -> Option<crate::simulator::ValidationRequest> {
        if let Some(i) = self.reqs.iter().position(|(_, k, _)| *k == key) {
            local_telemetry.sims_reqs_dropped += 1;
            if version > self.reqs[i].2 {
                self.reqs[i].2 = version;
                return Some(std::mem::replace(&mut self.reqs[i].0, req));
            }
            return Some(req);
        }
        self.reqs.push((req, key, version));
        local_telemetry.max_pending = local_telemetry.max_pending.max(self.reqs.len());
        None
    }

    fn next_req(&mut self) -> Option<crate::simulator::ValidationRequest> {
        let i = self
            .reqs
            .iter()
            .enumerate()
            .max_by_key(|(_, (r, _, _))| r.sort_key())
            .map(|(i, _)| i)?;
        Some(self.reqs.swap_remove(i).0)
    }

    /// Takes the backlog of simulations from the previous bid slot. They are never
    /// simulated, so the caller must answer them.
    /// All pending requests are always for `last_bid_slot` (checked on intake).
    fn drain(&mut self) -> Vec<crate::simulator::ValidationRequest> {
        self.reqs.drain(..).map(|(req, _, _)| req).collect()
    }
}

/// Pending merged-block requests. There's exactly one merge builder connection, so unlike
/// `PendingRequests` (keyed per-builder) we only keep the last request per base block.
struct PendingMergeRequests {
    reqs: Vec<(MergedValidationRequest, Arc<BlockMergeResponse>)>,
}

impl PendingMergeRequests {
    fn with_capacity(capacity: usize) -> Self {
        Self { reqs: Vec::with_capacity(capacity) }
    }

    /// Returns the evicted request if a newer one replaced it.
    fn store(
        &mut self,
        req: MergedValidationRequest,
        response: Arc<BlockMergeResponse>,
    ) -> Option<(MergedValidationRequest, Arc<BlockMergeResponse>)> {
        if let Some(i) =
            self.reqs.iter().position(|(r, _)| r.base_block_hash == req.base_block_hash)
        {
            if req.receive_ns > self.reqs[i].0.receive_ns {
                return Some(std::mem::replace(&mut self.reqs[i], (req, response)));
            }
            return None;
        }
        self.reqs.push((req, response));
        None
    }

    fn next_req(&mut self) -> Option<(MergedValidationRequest, Arc<BlockMergeResponse>)> {
        let i =
            self.reqs.iter().enumerate().max_by_key(|(_, (r, _))| r.receive_ns).map(|(i, _)| i)?;
        Some(self.reqs.swap_remove(i))
    }

    /// Clear backlog of simulations from the previous bid slot.
    fn clear(&mut self) {
        self.reqs.clear();
    }
}

/// Metric label for what a simulation said, so a builder's invalid-block rate can be read
/// apart from simulator trouble.
fn sim_outcome(error: Option<&BlockSimError>) -> &'static str {
    match error {
        None => "valid",
        Some(BlockSimError::PayloadTooLarge) => "too_large",
        Some(err) if err.is_sim_fault() => "sim_fault",
        Some(err) if err.is_temporary() => "temporary",
        Some(_) => "invalid",
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
        execution_payload: payload.clone(),
        blobs_bundle: response.blobs_bundle.clone(),
        execution_requests: response.execution_requests.clone(),
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
