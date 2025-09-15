use std::{
    self,
    time::{Duration, Instant},
};

use helix_common::{metrics::SimulatorMetrics, SimulatorConfig};
use helix_types::BlsPublicKeyBytes;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{error, warn};

use crate::{
    builder::{
        simulator::{client::SimulatorClient, SimulatorRequest},
        BlockMergeRequest, SimReponse,
    },
    service::SIMULATOR_REQUEST_TIMEOUT,
};

// TODO:
// - avoid sending blobs, and validate them here on a blocking task
// - send only block deltas
// - use SSZ not json
pub struct SimulatorManager {
    simulators: Vec<SimulatorClient>,
    requests: PendingRquests,
    join_set: JoinSet<SimReponse>,
    last_bid_slot: u64,
}

impl SimulatorManager {
    pub fn new(configs: Vec<SimulatorConfig>) -> Self {
        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let simulators = configs
            .into_iter()
            .map(|config| SimulatorClient::new(client.clone(), config))
            .collect();

        let requests = PendingRquests::with_capacity(200);
        let join_set = JoinSet::new();

        Self { simulators, requests, join_set, last_bid_slot: 0 }
    }

    pub async fn run(
        mut self,
        mut sim_requests_rx: mpsc::Receiver<SimulatorRequest>,
        mut merge_requests_rx: mpsc::Receiver<BlockMergeRequest>,
        is_local_dev: bool,
    ) {
        let (sync_tx, mut sync_rx) = mpsc::channel(1000);

        // sync monitor
        if !is_local_dev {
            let simulators = self.simulators.clone();
            tokio::spawn(async move {
                loop {
                    for (i, simulator) in simulators.iter().enumerate() {
                        let is_synced = simulator.is_synced().await.unwrap_or(false);
                        if sync_tx.send((i, is_synced)).await.is_err() {
                            error!("failed to send sync result to sim manager");
                        }
                        SimulatorMetrics::simulator_sync(simulator.endpoint(), is_synced);
                    }

                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }

        loop {
            tokio::select! {
                Some(req) = sim_requests_rx.recv() => self.handle_sim_request(req),
                Some(req) = merge_requests_rx.recv() => self.handle_merge_request(req),
                Some(Ok(resp)) = self.join_set.join_next(), if !self.join_set.is_empty()  => self.handle_task_response(resp),
                Some((id, is_synced)) = sync_rx.recv() => self.simulators[id].is_synced = is_synced,
            }
        }
    }

    fn handle_sim_request(&mut self, req: SimulatorRequest) {
        if req.bid_slot() > self.last_bid_slot {
            self.handle_new_slot(req.bid_slot());
        }

        if let Some(id) = self.next_sim_client() {
            self.spawn_sim(id, req)
        } else {
            self.requests.store(req)
        }
    }

    fn handle_merge_request(&mut self, req: BlockMergeRequest) {
        if let Some(id) = self.next_merge_client() {
            let client = &mut self.simulators[id];
            let to_send = client.merge_request_builder(&req);
            client.pending += 1;

            self.join_set.spawn(async move {
                let res = SimulatorClient::do_merge_request(to_send).await;
                let _ = req.res_tx.send(res);
                (id, None)
            });
        } else {
            warn!("no client available for merging! Dropping request");
        }
    }

    fn handle_task_response(&mut self, (id, paused_until): SimReponse) {
        let sim = &mut self.simulators[id];
        sim.pending = sim.pending.saturating_sub(1);
        sim.paused_until = sim.paused_until.max(paused_until); // keep highest pause

        if let Some(id) = self.next_sim_client() {
            if let Some(req) = self.requests.next_req() {
                self.spawn_sim(id, req);
            }
        }
    }

    fn spawn_sim(&mut self, id: usize, req: SimulatorRequest) {
        const PAUSE_DURATION: Duration = Duration::from_secs(60);

        let client = &mut self.simulators[id];
        let to_send = client.sim_request_builder(&req.request, req.is_top_bid);
        client.pending += 1;

        self.join_set.spawn(async move {
            let res = SimulatorClient::do_sim_request(to_send).await;
            let paused_until = if res.as_ref().is_err_and(|e| e.is_temporary()) {
                Some(Instant::now() + PAUSE_DURATION)
            } else {
                None
            };

            // ignore error if non-optimistic request timed out while being simulated
            let _ = req.res_tx.send(res);
            (id, paused_until)
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

    // TODO: local telemetry
    fn handle_new_slot(&mut self, bid_slot: u64) {
        self.last_bid_slot = bid_slot;
        self.requests.clear(bid_slot);
        let now = Instant::now();
        for s in self.simulators.iter_mut() {
            if s.paused_until.is_some_and(|until| until < now) {
                s.paused_until = None;
            }
        }
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

    fn store(&mut self, req: SimulatorRequest) {
        if let Some((i, _)) =
            self.builder_pubkeys.iter_mut().enumerate().find(|(_, r)| **r == *req.builder_pubkey())
        {
            if req.on_receive_ns > self.map[i].on_receive_ns {
                self.sort_keys[i] = req.sort_key();
                self.builder_pubkeys[i] = *req.builder_pubkey();
                self.map[i] = req;
            }
        } else {
            self.sort_keys.push(req.sort_key());
            self.builder_pubkeys.push(*req.builder_pubkey());
            self.map.push(req);
        }
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
