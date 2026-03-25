use std::{
    self,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use flux::{
    spine::SpineProducers as _,
    tile::{Tile, TileName},
    timing::Nanos,
};
use flux_utils::SharedVector;
use helix_common::{
    SimulatorConfig, SubmissionTrace,
    bid_submission::OptimisticVersion,
    chain_info::ChainInfo,
    is_local_dev,
    metrics::SimulatorMetrics,
    record_submission_step,
    simulator::{BlockSimError, JsonValidationRequest, SszValidationRequest},
    spawn_tracked,
    validator_preferences::{Filtering, ValidatorPreferences},
};
use helix_types::{BlsPublicKeyBytes, SimHydrationCache, SignedBidSubmission, Submission};
use ssz::Encode as _;
use tracing::{debug, error, info, warn};

use crate::{
    HelixSpine, SimRequest, ValidationRequest,
    auctioneer::Bid,
    bid_decoder::SubmissionDataWithSpan,
    simulator::{MergeRequest, SimResult, client::SimulatorClient},
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
    last_bid_slot: u64,
    local_telemetry: LocalTelemetry,
    /// Internal channel: async tasks notify the sim tile when work completes.
    task_tx: crossbeam_channel::Sender<SimTileInternalEvent>,
    rx: crossbeam_channel::Receiver<SimTileInternalEvent>,
    sim_requests: Arc<SharedVector<SimRequest>>,
    sim_results: Arc<SharedVector<SimResult>>,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
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
                SimTileInternalEvent::TaskDone { id, paused_until, result_ix } => {
                    self.handle_task_response(id, paused_until, result_ix, &mut adapter.producers);
                }
                SimTileInternalEvent::SyncStatus { id, is_synced } => {
                    self.handle_sync_status(id, is_synced);
                }
            }
        }

        // Consume inbound spine messages from the auctioneer.
        adapter.consume(|msg: ToSimMsg, _producers| match msg.kind {
            ToSimKind::Request => match self.sim_requests.get(msg.ix) {
                Some(payload) => match payload.as_ref() {
                    SimRequest::Validate { req, fast_track } => {
                        self.handle_sim_request((**req).clone(), *fast_track);
                    }
                    SimRequest::Merge(req) => {
                        self.handle_merge_request(req.clone());
                    }
                },
                None => error!(?msg, "sim inbound payload not found"),
            },
            ToSimKind::NewSlot => {
                self.on_new_slot(msg.bid_slot);
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
        chain_info: ChainInfo,
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

        if !is_local_dev() {
            let clients: Vec<SimulatorClient> =
                simulators.iter().map(|e| e.client.clone()).collect();
            spawn_tracked!({
                let sync_tx = task_tx.clone();
                async move {
                    loop {
                        for (id, simulator) in clients.iter().enumerate() {
                            let is_synced = simulator.is_synced().await.unwrap_or(false);
                            if sync_tx
                                .try_send(SimTileInternalEvent::SyncStatus { id, is_synced })
                                .is_err()
                            {
                                error!("failed to send sync status to sim tile");
                            }
                            SimulatorMetrics::simulator_sync(simulator.endpoint(), is_synced);
                        }

                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            });
        }

        let accept_optimistic = Arc::new(AtomicBool::new(true));
        let failsafe_triggered = Arc::new(AtomicBool::new(false));

        let ssz_sim_indices: Vec<usize> = simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.client.ssz_url.is_some())
            .map(|(i, _)| i)
            .collect();

        let tile = Self {
            simulators,
            ssz_sim_indices,
            requests,
            priority_requests,
            last_bid_slot: 0,
            local_telemetry: LocalTelemetry::default(),
            task_tx,
            rx,
            sim_requests,
            sim_results,
            decoded,
            hydration_cache: SimHydrationCache::new(),
            chain_info,
            accept_optimistic: accept_optimistic.clone(),
            failsafe_triggered: failsafe_triggered.clone(),
        };

        (accept_optimistic, failsafe_triggered, tile)
    }

    fn handle_sync_status(&mut self, id: usize, is_synced: bool) {
        self.simulators[id].is_synced = is_synced;
        let new = self.simulators.iter().any(|s| s.can_simulate_light());
        let prev = self.accept_optimistic.load(Ordering::Relaxed);
        if new != prev {
            warn!(prev, new, "changing accept_optimistic simulation status");
        }
        self.accept_optimistic.store(new, Ordering::Relaxed);
    }

    fn handle_sim_request(&mut self, req: crate::simulator::ValidationRequest, fast_track: bool) {
        let Some(decoded_data) = self.decoded.get(req.decoded_ix) else {
            error!(ix = req.decoded_ix, "decoded submission not found in ring");
            let result_ix = self.sim_results.push(SimResult::Validate((0, None)));
            producers.produce(FromSimMsg { ix: result_ix });
            return;
        };
        let builder_pubkey = *decoded_data.submission_data.submission.builder_pubkey();
        assert_eq!(decoded_data.submission_data.submission.bid_slot(), self.last_bid_slot);

        self.local_telemetry.sims_reqs += 1;

        let sim_id = self.select_simulator(&builder_pubkey);

        if let Some(id) = sim_id {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_sim(id, req)
        } else if fast_track {
            self.priority_requests.store(req, builder_pubkey, &mut self.local_telemetry)
        } else {
            self.requests.store(req, builder_pubkey, &mut self.local_telemetry)
        }
    }

    fn handle_merge_request(&mut self, req: MergeRequest) {
        self.local_telemetry.merge_reqs += 1;
        if let Some(id) = self.next_client(|s| s.can_merge()) {
            let sim = &mut self.simulators[id];
            let to_send = sim.client.merge_request_builder();
            sim.pending += 1;

            self.local_telemetry.max_in_flight =
                self.local_telemetry.max_in_flight.max(sim.pending);
            let timer = SimulatorMetrics::block_merge_timer(sim.client.endpoint());
            let task_tx = self.task_tx.clone();
            let sim_results = self.sim_results.clone();
            spawn_tracked!(async move {
                debug!(bid_slot = %req.bid_slot, block_hash = %req.block_hash, "sending merge request");
                let res = SimulatorClient::do_merge_request(&req, to_send).await;
                if res.is_ok() {
                    timer.stop_and_record();
                } else {
                    timer.stop_and_discard();
                }
                SimulatorMetrics::block_merge_status(res.is_ok());

                let result_ix = sim_results.push(SimResult::Merge((id, res)));
                let _ = task_tx.try_send(SimTileInternalEvent::TaskDone {
                    id,
                    paused_until: None,
                    result_ix,
                });
            });
        } else {
            self.local_telemetry.dropped_merge_reqs += 1;
            warn!("no client available for merging! Dropping request");
        }
    }

    fn handle_task_response(
        &mut self,
        id: usize,
        paused_until: Option<Instant>,
        result_ix: usize,
        producers: &mut HelixSpineProducers,
    ) {
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);
        sim.paused_until = sim.paused_until.max(paused_until); // keep highest pause

        producers.produce(FromSimMsg { ix: result_ix });

        if let Some(id) = self.next_client(|s| s.can_simulate()) &&
            let Some(req) = self.priority_requests.next_req().or(self.requests.next_req())
        {
            self.spawn_sim(id, req);
        }
    }

    fn spawn_sim(&mut self, id: usize, req: ValidationRequest) {
        const PAUSE_DURATION: Duration = Duration::from_secs(60);
        
        let Some(decoded_data) = self.decoded.get(req.decoded_ix) else {
            error!(ix = req.decoded_ix, "decoded submission not found in ring");
            // Balance pending so handle_task_response can route the next request.
            let sim = &mut self.simulators[id];
            sim.pending += 1;
            let result_ix = self.sim_results.push(SimResult::Validate((id, None)));
            let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                id,
                paused_until: None,
                result_ix,
            });
            return;
        };

        let (submission, tx_root) = match decoded_data.submission_data.submission.clone() {
            Submission::Full(s) => (s, None),
            Submission::Dehydrated(d) => {
                match self.hydration_cache.hydrate(d, self.chain_info.max_blobs_per_block()) {
                    Ok(h) => (h.submission, h.tx_root),
                    Err(e) => {
                        error!(%e, "hydration failed in sim tile");
                        let sim = &mut self.simulators[id];
                        sim.pending += 1;
                        let result_ix = self.sim_results.push(SimResult::Validate((id, None)));
                        let _ = self.task_tx.try_send(SimTileInternalEvent::TaskDone {
                            id,
                            paused_until: None,
                            result_ix,
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
            let (builder, method) = sim.client.sim_request_builder(submission.fork_name());
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

            let paused_until = if let Err(err) = res.as_ref() {
                SimulatorMetrics::sim_status(false);
                if err.is_temporary() { Some(Instant::now() + PAUSE_DURATION) } else { None }
            } else {
                SimulatorMetrics::sim_status(true);
                None
            };

            if let Some(got) = tx_root {
                let expected = submission.transactions_root();
                if expected != got {
                    res = Err(BlockSimError::InvalidTxRoot { got, expected })
                }
            }

            record_submission_step("simulation", start_sim.elapsed());

            let bid = Bid::new(version, &submission);
            let inner = SimulationResultInner {
                submission_ref,
                result: res,
                bid,
                trace,
                optimistic_version,
            };

            let result_ix = sim_results.push(SimResult::Validate((id, Some(inner))));
            let _ =
                task_tx.try_send(SimTileInternalEvent::TaskDone { id, paused_until, result_ix });
        });
    }

    /// Selection priority:
    /// 1. Sticky sim with SSZ endpoint (state locality + binary protocol)
    /// 2. Any SSZ-capable sim, least pending (binary protocol)
    /// 3. Any sim, least pending (JSON-RPC fallback; stickiness irrelevant without SSZ)
    fn select_simulator(&self, builder_pubkey: &BlsPublicKeyBytes) -> Option<usize> {
        if !self.ssz_sim_indices.is_empty() {
            let sticky =
                self.ssz_sim_indices[sticky_sim_index(self.ssz_sim_indices.len(), builder_pubkey)];
            if self.simulators[sticky].can_simulate() {
                return Some(sticky);
            }
            if let Some(id) = self
                .ssz_sim_indices
                .iter()
                .filter(|&&i| self.simulators[i].can_simulate())
                .min_by_key(|&&i| self.simulators[i].pending)
                .copied()
            {
                return Some(id);
            }
        }
        self.next_client(|s| s.can_simulate())
    }

    fn next_client(&self, pred: impl Fn(&SimEntry) -> bool) -> Option<usize> {
        self.simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| pred(s))
            .min_by_key(|(_, s)| s.pending)
            .map(|(i, _)| i)
    }

    fn on_new_slot(&mut self, bid_slot: u64) {
        if self.last_bid_slot > 0 {
            self.report();
        }

        self.last_bid_slot = bid_slot;
        self.requests.clear();
        self.priority_requests.clear();
        self.hydration_cache.clear();
        let now = Instant::now();
        for s in self.simulators.iter_mut() {
            if s.paused_until.is_some_and(|until| until < now) {
                s.paused_until = None;
            }
        }
    }

    fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);

        SimulatorMetrics::sim_mananger_count("sims_sent_immediately", tel.sims_sent_immediately);
        SimulatorMetrics::sim_mananger_count("sims_reqs_dropped", tel.sims_reqs_dropped);
        SimulatorMetrics::sim_mananger_count("stale_sim_reqs", tel.stale_sim_reqs);
        SimulatorMetrics::sim_manager_gauge("max_pending", tel.max_pending);
        SimulatorMetrics::sim_manager_gauge("max_in_flight", tel.max_in_flight);
        SimulatorMetrics::sim_mananger_count("merge_reqs", tel.merge_reqs);
        SimulatorMetrics::sim_mananger_count("dropped_merge_reqs", tel.dropped_merge_reqs);

        info!(
            bid_slot = self.last_bid_slot,
            sims_reqs = tel.sims_reqs,
            sims_sent_immediately = tel.sims_sent_immediately,
            sims_reqs_dropped = tel.sims_reqs_dropped,
            stale_sim_reqs = tel.stale_sim_reqs,
            max_pending = tel.max_pending,
            max_in_flight = tel.max_in_flight,
            merge_reqs = tel.merge_reqs,
            dropped_merge_reqs = tel.dropped_merge_reqs,
            "sim manager telemetry"
        )
    }
}

struct SimEntry {
    client: SimulatorClient,
    is_synced: bool,
    /// For certain errors we pause sims for some time to allow time for the node to recover
    paused_until: Option<Instant>,
    /// Current number of pending tasks (validation or merging)
    pending: usize,
}

impl SimEntry {
    fn new(client: SimulatorClient) -> Self {
        Self { client, is_synced: false, paused_until: None, pending: 0 }
    }

    /// A lighter check to decide whether we should accept optimistic submissions
    fn can_simulate_light(&self) -> bool {
        self.is_synced &&
            match self.paused_until {
                Some(until) => Instant::now() > until,
                None => true,
            }
    }

    fn can_simulate(&self) -> bool {
        self.can_simulate_light() && self.pending < self.client.config.max_concurrent_tasks
    }

    fn can_merge(&self) -> bool {
        self.can_simulate() && self.client.config.is_merging_simulator
    }
}

pub(crate) const SIMULATOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

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
    merge_reqs: usize,
    dropped_merge_reqs: usize,
}

// Sim id / Simulation Result, so we can use this for merging requests
pub type ValidationResult = (usize, Option<SimulationResultInner>);
#[derive(Clone)]
pub struct SimulationResultInner {
    pub result: Result<(), BlockSimError>,
    pub submission_ref: crate::auctioneer::SubmissionRef,
    pub bid: Bid,
    pub trace: SubmissionTrace,
    pub optimistic_version: OptimisticVersion,
}

enum SimDispatch {
    Ssz { to_send: reqwest::RequestBuilder, ssz_url: String, http: reqwest::Client },
    Json { to_send: reqwest::RequestBuilder, method: String },
}

/// Internal-only events: async task → sim tile (not tile-to-tile).
pub(super) enum SimTileInternalEvent {
    TaskDone { id: usize, paused_until: Option<Instant>, result_ix: usize },
    SyncStatus { id: usize, is_synced: bool },
}

/// Jump consistent hash — maps a builder pubkey to a simulator index with
/// minimal reassignment when the set size changes.
fn sticky_sim_index(num_simulators: usize, builder_pubkey: &BlsPublicKeyBytes) -> usize {
    if num_simulators <= 1 {
        return 0;
    }
    let key = u64::from_le_bytes(builder_pubkey.0[..8].try_into().unwrap());
    jump_hash(key, num_simulators)
}

/// Stateless consistent hash — minimises slot reassignment as `n` changes.
/// <https://arxiv.org/abs/1406.2294>
fn jump_hash(mut key: u64, n: usize) -> usize {
    let mut b: i64 = -1;
    let mut j: i64 = 0;
    while j < n as i64 {
        b = j;
        key = key.wrapping_mul(2862933555777941757).wrapping_add(1);
        j = ((b + 1) as f64 * ((1u64 << 31) as f64) / ((key >> 33) + 1) as f64) as i64;
    }
    b as usize
}

/// Pending requests, we only keep the last one for each builder.
struct PendingRequests {
    reqs: Vec<(crate::simulator::ValidationRequest, BlsPublicKeyBytes)>,
}

impl PendingRequests {
    fn with_capacity(capacity: usize) -> Self {
        Self { reqs: Vec::with_capacity(capacity) }
    }

    fn store(
        &mut self,
        req: crate::simulator::ValidationRequest,
        builder_pubkey: BlsPublicKeyBytes,
        local_telemetry: &mut LocalTelemetry,
    ) {
        if let Some(i) = self.reqs.iter().position(|(_, pk)| *pk == builder_pubkey) {
            if req.on_receive_ns() > self.reqs[i].0.on_receive_ns() {
                self.reqs[i].0 = req;
            }
            local_telemetry.sims_reqs_dropped += 1;
        } else {
            self.reqs.push((req, builder_pubkey));
        }
        local_telemetry.max_pending = local_telemetry.max_pending.max(self.reqs.len());
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

fn create_ssz_request(
    req: &ValidationRequest,
    submission: &SignedBidSubmission,
) -> SszValidationRequest {
    SszValidationRequest {
        apply_blacklist: req.apply_blacklist,
        registered_gas_limit: req.registered_gas_limit,
        parent_beacon_block_root: req.parent_beacon_block_root,
        inclusion_list: req.inclusion_list.clone(),
        decoder_params: None,
        signed_bid_submission: submission.as_ssz_bytes(),
    }
}
