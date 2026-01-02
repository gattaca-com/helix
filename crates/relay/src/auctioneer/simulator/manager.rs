use std::{
    self,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use helix_common::{
    SimulatorConfig, SubmissionTrace, bid_submission::OptimisticVersion, is_local_dev,
    metrics::SimulatorMetrics, record_submission_step, simulator::BlockSimError, spawn_tracked,
};
use helix_types::{BlsPublicKeyBytes, SignedBidSubmission, SubmissionVersion};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::{
    api::service::SIMULATOR_REQUEST_TIMEOUT,
    auctioneer::{
        simulator::{BlockMergeRequest, SimulatorRequest, client::SimulatorClient},
        types::{Event, SubmissionResult},
    },
};

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
pub type SimulationResult = (usize, Option<SimulationResultInner>);
pub struct SimulationResultInner {
    pub result: Result<(), BlockSimError>,
    // Some if not optimistic
    pub res_tx: Option<oneshot::Sender<SubmissionResult>>,
    // TODO: move up
    pub paused_until: Option<Instant>,
    pub submission: SignedBidSubmission,
    pub trace: SubmissionTrace,
    pub optimistic_version: OptimisticVersion,
    pub version: SubmissionVersion,
}

// TODO:
// - avoid sending blobs, and validate them here on a blocking task
// - send only block deltas
// - use SSZ not json
pub struct SimulatorManager {
    simulators: Vec<SimulatorClient>,
    requests: PendingRquests,
    priority_requests: PendingRquests,
    last_bid_slot: u64,
    local_telemetry: LocalTelemetry,
    sim_result_tx: crossbeam_channel::Sender<Event>,
    /// If we have any synced simulator
    accept_optimistic: bool,
    /// If we failed to demote a builder in the DB
    pub failsafe_triggered: Arc<AtomicBool>,
}

impl SimulatorManager {
    pub fn new(
        configs: Vec<SimulatorConfig>,
        sim_result_tx: crossbeam_channel::Sender<Event>,
    ) -> Self {
        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let simulators: Vec<_> = configs
            .into_iter()
            .map(|config| SimulatorClient::new(client.clone(), config))
            .collect();

        let requests = PendingRquests::with_capacity(200);
        let priority_requests = PendingRquests::with_capacity(30);

        if !is_local_dev() {
            spawn_tracked!({
                let sync_tx = sim_result_tx.clone();
                let simulators = simulators.clone();
                async move {
                    loop {
                        for (id, simulator) in simulators.iter().enumerate() {
                            let is_synced = simulator.is_synced().await.unwrap_or(false);
                            if sync_tx.try_send(Event::SimulatorSync { id, is_synced }).is_err() {
                                error!("failed to send sync result to sim manager");
                            }
                            SimulatorMetrics::simulator_sync(simulator.endpoint(), is_synced);
                        }

                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            });
        }

        Self {
            simulators,
            requests,
            priority_requests,

            last_bid_slot: 0,
            local_telemetry: LocalTelemetry::default(),
            sim_result_tx,

            accept_optimistic: true,
            failsafe_triggered: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn can_process_optimistic_submission(&self) -> bool {
        self.accept_optimistic && !self.failsafe_triggered.load(Ordering::Relaxed)
    }

    pub fn handle_sync_status(&mut self, id: usize, is_synced: bool) {
        self.simulators[id].is_synced = is_synced;
        let new = self.simulators.iter().any(|s| s.can_simulate_light());
        let prev = self.accept_optimistic;
        if new != prev {
            warn!(prev, new, "changing accept_optimistic simulation status");
        }
        self.accept_optimistic = new;
    }

    pub fn handle_sim_request(&mut self, req: SimulatorRequest, fast_track: bool) {
        assert_eq!(req.bid_slot(), self.last_bid_slot);

        self.local_telemetry.sims_reqs += 1;
        if let Some(id) = self.next_sim_client() {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_sim(id, req)
        } else if fast_track {
            self.priority_requests.store(req, &mut self.local_telemetry)
        } else {
            self.requests.store(req, &mut self.local_telemetry)
        }
    }

    pub fn handle_merge_request(&mut self, req: BlockMergeRequest) {
        self.local_telemetry.merge_reqs += 1;
        if let Some(id) = self.next_merge_client() {
            let client = &mut self.simulators[id];
            let to_send = client.merge_request_builder();
            client.pending += 1;

            self.local_telemetry.max_in_flight =
                self.local_telemetry.max_in_flight.max(client.pending);
            let timer = SimulatorMetrics::block_merge_timer(client.endpoint());
            let tx = self.sim_result_tx.clone();
            spawn_tracked!(async move {
                debug!(bid_slot = %req.bid_slot, block_hash = %req.block_hash, "sending merge request");
                let res = SimulatorClient::do_merge_request(&req, to_send).await;
                timer.stop_and_record();
                SimulatorMetrics::block_merge_status(res.is_ok());

                let result = (id, res);

                let _ = tx.try_send(Event::MergeResult(result));
            });
        } else {
            self.local_telemetry.dropped_merge_reqs += 1;
            warn!("no client available for merging! Dropping request");
        }
    }

    pub fn handle_task_response(&mut self, id: usize, paused_until: Option<Instant>) {
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);
        sim.paused_until = sim.paused_until.max(paused_until); // keep highest pause

        if let Some(id) = self.next_sim_client() &&
            let Some(req) = self.priority_requests.next_req().or(self.requests.next_req())
        {
            self.spawn_sim(id, req);
        }
    }

    pub fn spawn_sim(&mut self, id: usize, req: SimulatorRequest) {
        const PAUSE_DURATION: Duration = Duration::from_secs(60);

        let client = &mut self.simulators[id];
        let (to_send, sim_method) = client.sim_request_builder(req.submission.fork_name());
        client.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(client.pending);
        let timer = SimulatorMetrics::timer(client.endpoint());
        let tx = self.sim_result_tx.clone();
        spawn_tracked!(async move {
            let start_sim = Instant::now();
            let block_hash = req.submission.block_hash();
            debug!(%block_hash, "sending simulation request");

            let optimistic_version = req.optimistic_version();
            SimulatorMetrics::sim_count(optimistic_version.is_optimistic());
            let mut res =
                SimulatorClient::do_sim_request(&req.request, req.is_top_bid, sim_method, to_send)
                    .await;
            let time = timer.stop_and_record();

            debug!(%block_hash, time_secs = time, ?res, "simulation completed");

            let paused_until = if let Err(err) = res.as_ref() {
                SimulatorMetrics::sim_status(false);
                if err.is_temporary() { Some(Instant::now() + PAUSE_DURATION) } else { None }
            } else {
                SimulatorMetrics::sim_status(true);
                None
            };

            if let Some(got) = req.tx_root {
                let expected = req.submission.transactions_root();

                if expected != got {
                    res = Err(BlockSimError::InvalidTxRoot { got, expected })
                }
            }

            record_submission_step("simulation", start_sim.elapsed());

            let result = (
                id,
                Some(SimulationResultInner {
                    result: res,
                    paused_until,
                    res_tx: req.res_tx,
                    submission: req.submission,
                    trace: req.trace,
                    optimistic_version,
                    version: req.version,
                }),
            );

            let _ = tx.try_send(Event::SimResult(result));
        });
    }

    fn next_sim_client(&self) -> Option<usize> {
        self.simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.can_simulate())
            .min_by_key(|(_, s)| s.pending)
            .map(|(i, _)| i)
    }

    fn next_merge_client(&self) -> Option<usize> {
        self.simulators
            .iter()
            .enumerate()
            .filter(|(_, s)| s.can_merge())
            .min_by_key(|(_, s)| s.pending)
            .map(|(i, _)| i)
    }

    pub fn on_new_slot(&mut self, bid_slot: u64) {
        if self.last_bid_slot > 0 {
            self.report();
        }

        self.last_bid_slot = bid_slot;
        self.requests.clear(bid_slot);
        self.priority_requests.clear(bid_slot);
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

/// Pending requests, we only keep the last one for each builder
struct PendingRquests {
    map: Vec<SimulatorRequest>,
    sort_keys: Vec<(u8, u8, u64)>,
    builder_pubkeys: Vec<BlsPublicKeyBytes>,
}

impl PendingRquests {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            map: Vec::with_capacity(capacity),
            builder_pubkeys: Vec::with_capacity(capacity),
            sort_keys: Vec::with_capacity(capacity),
        }
    }

    fn store(&mut self, req: SimulatorRequest, local_telemetry: &mut LocalTelemetry) {
        if let Some((i, _)) =
            self.builder_pubkeys.iter_mut().enumerate().find(|(_, r)| **r == *req.builder_pubkey())
        {
            if req.on_receive_ns() > self.map[i].on_receive_ns() {
                self.sort_keys[i] = req.sort_key();
                self.builder_pubkeys[i] = *req.builder_pubkey();
                self.map[i] = req;
            }

            local_telemetry.sims_reqs_dropped += 1;
        } else {
            self.sort_keys.push(req.sort_key());
            self.builder_pubkeys.push(*req.builder_pubkey());
            self.map.push(req);
        }

        local_telemetry.max_pending = local_telemetry.max_pending.max(self.map.len())
    }

    fn next_req(&mut self) -> Option<SimulatorRequest> {
        let i = self.sort_keys.iter().enumerate().max_by_key(|(_, r)| *r).map(|(i, _)| i)?;

        self.sort_keys.swap_remove(i);
        self.builder_pubkeys.swap_remove(i);
        Some(self.map.swap_remove(i))
    }

    /// Clear backlog of simulations from the previous bid slot, this closes all optimistic
    /// submissions and non-optimistic ones which have timed out
    fn clear(&mut self, bid_slot: u64) {
        let mut i = 0;

        while i < self.map.len() {
            let req = &self.map[i];
            if req.is_closed() || req.bid_slot() < bid_slot {
                self.sort_keys.swap_remove(i);
                self.builder_pubkeys.swap_remove(i);
                self.map.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}
