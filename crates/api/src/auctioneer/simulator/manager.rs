use std::{
    self,
    time::{Duration, Instant},
};

use helix_common::{
    bid_submission::BidSubmission, metrics::SimulatorMetrics, simulator::BlockSimError,
    SimulatorConfig,
};
use helix_types::{BlockMergingPreferences, BlsPublicKeyBytes, SignedBidSubmission};
use tokio::{runtime::Handle, sync::oneshot, task::JoinSet};
use tracing::{debug, info, warn};

use crate::{
    auctioneer::{
        simulator::{client::SimulatorClient, BlockMergeRequest, SimReponse, SimulatorRequest},
        types::SubmissionResult,
        Event,
    },
    service::SIMULATOR_REQUEST_TIMEOUT,
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

pub struct SimulationResult {
    pub result: Result<(), BlockSimError>,
    // Some if not optimistic
    pub res_tx: Option<oneshot::Sender<SubmissionResult>>,
    pub id: usize,
    // TODO: move up
    pub paused_until: Option<Instant>,
    pub submission: SignedBidSubmission,
    pub merging_preferences: BlockMergingPreferences,
}

impl SimulationResult {
    pub fn was_optimistic_sim(&self) -> bool {
        self.res_tx.is_none()
    }
}

// TODO:
// - avoid sending blobs, and validate them here on a blocking task
// - send only block deltas
// - use SSZ not json
pub struct SimulatorManager {
    simulators: Vec<SimulatorClient>,
    requests: PendingRquests,
    join_set: JoinSet<SimReponse>,
    last_bid_slot: u64,
    local_telemetry: LocalTelemetry,
    // TODO: Use this:
    accept_optimistic: bool,
    sim_result_tx: crossbeam_channel::Sender<Event>,
    pub runtime: Handle,
}

impl SimulatorManager {
    pub fn new(
        configs: Vec<SimulatorConfig>,
        sim_result_tx: crossbeam_channel::Sender<Event>,
        runtime: Handle,
    ) -> Self {
        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let simulators = configs
            .into_iter()
            .map(|config| SimulatorClient::new(client.clone(), config))
            .collect();

        let requests = PendingRquests::with_capacity(200);
        let join_set = JoinSet::new();

        Self {
            simulators,
            requests,
            join_set,
            last_bid_slot: 0,
            local_telemetry: LocalTelemetry::default(),
            accept_optimistic: true,
            sim_result_tx,
            runtime,
        }
    }

    // pub async fn start_sync_monitor(&self, is_local_dev: bool) {
    //     // sync monitor
    //     if !is_local_dev {
    //         let simulators = self.simulators.clone();
    //         tokio::spawn(async move {
    //             loop {
    //                 for (i, simulator) in simulators.iter().enumerate() {
    //                     let is_synced = simulator.is_synced().await.unwrap_or(false);
    //                     if sync_tx.send((i, is_synced)).await.is_err() {
    //                         error!("failed to send sync result to sim manager");
    //                     }
    //                     SimulatorMetrics::simulator_sync(simulator.endpoint(), is_synced);
    //                 }

    //                 tokio::time::sleep(Duration::from_secs(1)).await;
    //             }
    //         });
    //     }
    // }

    pub fn handle_sync_status(&mut self, id: usize, is_synced: bool) {
        self.simulators[id].is_synced = is_synced;
        let new = self.simulators.iter().any(|s| s.can_simulate_light());
        let prev = self.accept_optimistic;
        if new != prev {
            warn!(prev, new, "changing optimistic simulation status");
        }
        self.accept_optimistic = new;
    }

    pub fn handle_sim_request(&mut self, req: SimulatorRequest) {
        assert_eq!(req.bid_slot(), self.last_bid_slot);

        self.local_telemetry.sims_reqs += 1;
        if let Some(id) = self.next_sim_client() {
            self.local_telemetry.sims_sent_immediately += 1;
            self.spawn_sim(id, req)
        } else {
            self.requests.store(req, &mut self.local_telemetry)
        }
    }

    pub fn handle_merge_request(&mut self, req: BlockMergeRequest) {
        self.local_telemetry.merge_reqs += 1;
        if let Some(id) = self.next_merge_client() {
            let client = &mut self.simulators[id];
            let to_send = client.merge_request_builder(&req);
            client.pending += 1;

            self.local_telemetry.max_in_flight =
                self.local_telemetry.max_in_flight.max(client.pending);
            let timer = SimulatorMetrics::block_merge_timer(client.endpoint());
            self.join_set.spawn_on(
                async move {
                    debug!(block_hash =% req.block_hash, "sending merge request");
                    let res = SimulatorClient::do_merge_request(to_send).await;
                    timer.stop_and_record();
                    SimulatorMetrics::block_merge_status(res.is_ok());

                    let _ = req.res_tx.send(res);
                    (id, None)
                },
                &self.runtime,
            );
        } else {
            self.local_telemetry.dropped_merge_reqs += 1;
            warn!("no client available for merging! Dropping request");
        }
    }

    pub fn handle_task_response(&mut self, id: usize, paused_until: Option<Instant>) {
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);
        sim.paused_until = sim.paused_until.max(paused_until); // keep highest pause

        if let Some(id) = self.next_sim_client() {
            if let Some(req) = self.requests.next_req() {
                self.spawn_sim(id, req);
            }
        }
    }

    pub fn spawn_sim(&mut self, id: usize, req: SimulatorRequest) {
        const PAUSE_DURATION: Duration = Duration::from_secs(60);

        let client = &mut self.simulators[id];
        let to_send = client.sim_request_builder(&req.request, req.is_top_bid);
        client.pending += 1;

        self.local_telemetry.max_in_flight = self.local_telemetry.max_in_flight.max(client.pending);
        let timer = SimulatorMetrics::timer(client.endpoint());
        let tx = self.sim_result_tx.clone();
        self.runtime.spawn(async move {
            let block_hash = req.submission.block_hash();
            debug!(%block_hash, "sending simulation request");

            SimulatorMetrics::sim_count(req.is_optimistic);
            let res = SimulatorClient::do_sim_request(to_send).await;
            let time = timer.stop_and_record();

            debug!(%block_hash, time_secs = time, ?res, "simulation completed");

            let paused_until = if let Err(err) = res.as_ref() {
                SimulatorMetrics::sim_status(false);
                if err.is_temporary() {
                    Some(Instant::now() + PAUSE_DURATION)
                } else {
                    None
                }
            } else {
                SimulatorMetrics::sim_status(true);
                None
            };

            let result = SimulationResult {
                result: res,
                paused_until,
                id,
                res_tx: req.res_tx,
                submission: req.submission,
                merging_preferences: req.merging_preferences,
            };

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
        self.report();
        self.last_bid_slot = bid_slot;
        self.requests.clear(bid_slot);
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
            if req.on_receive_ns > self.map[i].on_receive_ns {
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
