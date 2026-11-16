use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use bytes::Bytes;
use flux::{spine::SpineAdapter, tile::Tile, timing::Nanos};
use flux_network::{
    Network, Token,
    http::{Failure, HttpEvent, HttpNetwork, HttpPool, HttpResponse, RequestId},
};
use helix_common::{
    SimulatorConfig,
    decoder::SubmissionDecoderParams,
    http::client::HttpClient,
    metrics::SimulatorMetrics,
    record_submission_step,
    simulator::{
        BlockSimError, JsonValidationRequest, MergedJsonValidationRequest,
        SszMergedValidationRequestRef, SszValidationRequestRef, TxDetail,
    },
    validator_preferences::{Filtering, ValidatorPreferences},
};
use helix_types::{ForkName, GloasBidTrace, SignedBidSubmission};
use prometheus::HistogramTimer;
use ssz::Encode as _;
use tracing::{debug, error};
use url::{Position, Url};
use uuid::Uuid;

use super::{
    BlockMergeResponse, MIN_LIMIT, MergedSimulationResultInner, MergedValidationRequest,
    SIMULATOR_REQUEST_TIMEOUT, SimResult, SimulationResultInner, SimulatorsEvent,
    ValidationRequest,
    client::{JsonRpcRequest, SYNC_REQUEST, SimulatorClient},
    merged_block_to_submission, sim_outcome, ssz_request,
};
use crate::{HelixSpine, auctioneer::Bid, block_merging::merged_gloas_submission};

const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Bounds one request or response body. A JSON submission with its blobs runs to tens of
/// megabytes.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;
/// A simulator may close a connection it has not heard from in a while. Until that one
/// is back the pool is a connection short, so it is reopened well within a simulation's
/// run time.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(200);
const JSON: &str = "application/json";
const OCTET_STREAM: &str = "application/octet-stream";

/// Work the simulator pool hands to the tile. The pool has already counted
/// it against simulator `id`, so each job is answered with exactly one
/// `SimulatorsEvent::TaskDone`.
// Stored inline; boxing would add a heap alloc on the hot path.
#[allow(clippy::large_enum_variant)]
pub(super) enum SimJob {
    Validate {
        id: usize,
        req: ValidationRequest,
        timer: HistogramTimer,
    },
    Merged {
        id: usize,
        req: MergedValidationRequest,
        response: Arc<BlockMergeResponse>,
        timer: HistogramTimer,
    },
}

/// Encodes simulation requests and exchanges them with the simulators, off the
/// auctioneer's core. The pool in `Simulators` decides what runs where; this
/// tile only does the encoding and the I/O.
pub struct SimulatorTile {
    net: Network,
    sims: Vec<Simulator>,
    jobs: crossbeam_channel::Receiver<SimJob>,
    done: crossbeam_channel::Sender<SimulatorsEvent>,
    in_flight: Vec<InFlight>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Rpc,
    Ssz,
}

struct Simulator {
    client: SimulatorClient,
    /// Sync polls, and simulations when the simulator has no SSZ endpoint.
    rpc: Option<Endpoint>,
    rpc_path: String,
    ssz: Option<Endpoint>,
    validate_path: String,
    validate_merged_path: String,
    /// `None` while a sync poll is in flight.
    next_sync: Option<Instant>,
}

struct Endpoint {
    http: HttpNetwork,
    pool: HttpPool,
    /// Flux queues a request for a pool with no live connection and starts its
    /// deadline only once it is written, so a request for a simulator that is
    /// down would never be answered. Tracking the live connections lets such a
    /// request fail at once instead.
    connected: Vec<Token>,
    host: String,
}

struct InFlight {
    sim: usize,
    route: Route,
    request: RequestId,
    work: Work,
}

// Stored inline; boxing would add a heap alloc on the hot path.
#[allow(clippy::large_enum_variant)]
enum Work {
    Sync,
    Validate(Validation),
    Merged(MergedValidation),
}

struct Validation {
    req: ValidationRequest,
    timer: HistogramTimer,
    started: Nanos,
    /// The SSZ request, kept to resend on a hydration miss. Empty on the JSON route.
    body: Bytes,
    retried: bool,
    /// Set by a hydration miss: the entry has no request out until it is sent again.
    resend: bool,
}

struct MergedValidation {
    submission_id: Uuid,
    block_hash: B256,
    timer: HistogramTimer,
    started: Nanos,
    request_len: usize,
}

type Outcome<'a, 'b> = Result<&'a HttpResponse<'b>, Failure>;

impl SimulatorTile {
    pub(super) fn new(
        configs: &[SimulatorConfig],
        jobs: crossbeam_channel::Receiver<SimJob>,
        done: crossbeam_channel::Sender<SimulatorsEvent>,
    ) -> Self {
        let mut net = Network::default();
        let now = Instant::now();
        let sims = configs
            .iter()
            .map(|config| {
                let client = SimulatorClient::new(config.clone());
                let concurrency = config.max_concurrent_tasks.max(MIN_LIMIT);
                let ssz_url = client.ssz_url.as_deref();
                // One connection is always left for the sync poll.
                let rpc_connections = if ssz_url.is_some() { 1 } else { concurrency + 1 };
                Simulator {
                    rpc: Endpoint::open(&mut net, &config.url, rpc_connections),
                    rpc_path: Endpoint::path(&config.url),
                    ssz: ssz_url.and_then(|url| Endpoint::open(&mut net, url, concurrency)),
                    validate_path: ssz_url
                        .map(|url| Endpoint::path(&format!("{url}/validate")))
                        .unwrap_or_default(),
                    validate_merged_path: ssz_url
                        .map(|url| Endpoint::path(&format!("{url}/validate_merged")))
                        .unwrap_or_default(),
                    next_sync: Some(now),
                    client,
                }
            })
            .collect();

        Self { net, sims, jobs, done, in_flight: Vec::new() }
    }

    /// One turn of the tile; returns whether it did anything.
    fn poll(&mut self) -> bool {
        let mut worked = false;
        while let Ok(job) = self.jobs.try_recv() {
            worked = true;
            match job {
                SimJob::Validate { id, req, timer } => self.start_validation(id, req, timer),
                SimJob::Merged { id, req, response, timer } => {
                    self.start_merged(id, req, &response, timer)
                }
            }
        }

        self.poll_sync(Instant::now());

        let sims = &mut self.sims;
        worked |= self.net.poll_with(|event| {
            for sim in sims.iter_mut() {
                if sim.rpc.as_mut().is_some_and(|endpoint| endpoint.http.on_event(&event)) ||
                    sim.ssz.as_mut().is_some_and(|endpoint| endpoint.http.on_event(&event))
                {
                    return;
                }
            }
        });

        for id in 0..self.sims.len() {
            self.drive(id, Route::Rpc);
            self.drive(id, Route::Ssz);
        }
        worked
    }

    fn start_validation(&mut self, id: usize, req: ValidationRequest, timer: HistogramTimer) {
        let started = Nanos::now();
        debug!(block_hash = %req.submission.block_hash(), "sending simulation request");
        SimulatorMetrics::sim_count(req.is_optimistic);

        let sim = &mut self.sims[id];
        let mut validation =
            Validation { req, timer, started, body: Bytes::new(), retried: false, resend: false };
        let req = &validation.req;
        let (route, sent) = if sim.client.ssz_url.is_some() {
            let body = ssz_request(
                req.apply_blacklist,
                req.registered_gas_limit,
                req.parent_beacon_block_root,
                &req.inclusion_list,
                &req.submission,
            );
            let sent = sim.ssz.as_mut().and_then(|endpoint| {
                endpoint.send(
                    &mut self.net,
                    &sim.validate_path,
                    OCTET_STREAM,
                    req.is_top_bid,
                    &body,
                )
            });
            validation.body = body;
            (Route::Ssz, sent)
        } else {
            let fork = req.submission.fork_name();
            let (SignedBidSubmission::Fulu(submission), Some(method)) =
                (&req.submission, sim.client.json_method(fork))
            else {
                validation.finish(id, Err(BlockSimError::UnsupportedFork(fork)), &self.done);
                return;
            };
            let filtering =
                if req.apply_blacklist { Filtering::Regional } else { Filtering::Global };
            let json_req = JsonValidationRequest::new(
                req.registered_gas_limit,
                submission,
                ValidatorPreferences { filtering, ..Default::default() },
                req.parent_beacon_block_root,
                &req.inclusion_list,
            );
            let body = serde_json::to_vec(&JsonRpcRequest::new(method, &json_req))
                .expect("a validation request serializes");
            let sent = sim.rpc.as_mut().and_then(|endpoint| {
                endpoint.send(&mut self.net, &sim.rpc_path, JSON, req.is_top_bid, &body)
            });
            (Route::Rpc, sent)
        };

        match sent {
            Some(request) => self.in_flight.push(InFlight {
                sim: id,
                route,
                request,
                work: Work::Validate(validation),
            }),
            None => {
                error!(endpoint = sim.client.endpoint(), "simulator unreachable");
                validation.finish(id, Err(BlockSimError::RpcError), &self.done);
            }
        }
    }

    fn start_merged(
        &mut self,
        id: usize,
        req: MergedValidationRequest,
        response: &BlockMergeResponse,
        timer: HistogramTimer,
    ) {
        let started = Nanos::now();
        let block_hash = response.execution_payload.block_hash;
        let base_payment_tx_index = response.base_payment_tx_index as u64;
        let gloas = !response.block_access_list.is_empty();
        let submission = merged_block_to_submission(response, &req);
        let sim = &mut self.sims[id];
        let ssz = sim.client.ssz_url.is_some();

        // A Gloas block has only the SSZ route.
        let gloas_bytes = match (gloas, ssz) {
            (true, true) => {
                let message = GloasBidTrace {
                    trace: submission.message.clone(),
                    parent_block_root: B256::ZERO,
                };
                merged_gloas_submission(response, message)
                    .map(|s| s.as_bytes().to_vec())
                    .ok_or(BlockSimError::UnsupportedFork(ForkName::Gloas))
            }
            (true, false) => Err(BlockSimError::UnsupportedFork(ForkName::Gloas)),
            (false, _) => Ok(Vec::new()),
        };
        let gloas_bytes = match gloas_bytes {
            Ok(bytes) => bytes,
            Err(err) => {
                let inner = MergedSimulationResultInner {
                    result: Err(err),
                    submission_id: req.submission_id,
                    block_hash,
                    txs: Vec::new(),
                };
                let _ = self.done.send(SimulatorsEvent::TaskDone {
                    id,
                    error: None,
                    result: Box::new(SimResult::ValidateMerged((id, Some(inner)))),
                    elapsed: None,
                });
                return;
            }
        };
        debug!(%block_hash, "sending merged block simulation request");
        SimulatorMetrics::sim_count(false);

        let (route, request_len, sent) = if ssz {
            let body = if gloas {
                SszMergedValidationRequestRef {
                    base: SszValidationRequestRef {
                        apply_blacklist: req.apply_blacklist,
                        registered_gas_limit: req.registered_gas_limit,
                        parent_beacon_block_root: req.parent_beacon_block_root,
                        inclusion_list: &req.inclusion_list,
                        decoder_params: Some(SubmissionDecoderParams::plain(ForkName::Gloas)),
                        signed_bid_submission: &gloas_bytes,
                    },
                    base_payment_tx_index,
                }
                .as_ssz_bytes()
            } else {
                SszMergedValidationRequestRef {
                    base: SszValidationRequestRef {
                        apply_blacklist: req.apply_blacklist,
                        registered_gas_limit: req.registered_gas_limit,
                        parent_beacon_block_root: req.parent_beacon_block_root,
                        inclusion_list: &req.inclusion_list,
                        decoder_params: None,
                        signed_bid_submission: &submission,
                    },
                    base_payment_tx_index,
                }
                .as_ssz_bytes()
            };
            let sent = sim.ssz.as_mut().and_then(|endpoint| {
                endpoint.send(&mut self.net, &sim.validate_merged_path, OCTET_STREAM, false, &body)
            });
            (Route::Ssz, body.len(), sent)
        } else {
            let filtering =
                if req.apply_blacklist { Filtering::Regional } else { Filtering::Global };
            let json_req = MergedJsonValidationRequest {
                base: JsonValidationRequest::new(
                    req.registered_gas_limit,
                    &submission,
                    ValidatorPreferences { filtering, ..Default::default() },
                    req.parent_beacon_block_root,
                    &req.inclusion_list,
                ),
                base_payment_tx_index,
            };
            let body = serde_json::to_vec(&JsonRpcRequest::new(
                &sim.client.sim_method_merged_v5,
                &json_req,
            ))
            .expect("a validation request serializes");
            let sent = sim.rpc.as_mut().and_then(|endpoint| {
                endpoint.send(&mut self.net, &sim.rpc_path, JSON, false, &body)
            });
            (Route::Rpc, body.len(), sent)
        };

        let merged = MergedValidation {
            submission_id: req.submission_id,
            block_hash,
            timer,
            started,
            request_len,
        };
        match sent {
            Some(request) => self.in_flight.push(InFlight {
                sim: id,
                route,
                request,
                work: Work::Merged(merged),
            }),
            None => {
                error!(endpoint = sim.client.endpoint(), "simulator unreachable");
                merged.finish(id, Err(BlockSimError::RpcError), &self.done);
            }
        }
    }

    fn poll_sync(&mut self, now: Instant) {
        for (id, sim) in self.sims.iter_mut().enumerate() {
            if sim.next_sync.is_none_or(|at| now < at) {
                continue;
            }
            let sent = sim.rpc.as_mut().and_then(|endpoint| {
                endpoint.send(&mut self.net, &sim.rpc_path, JSON, false, SYNC_REQUEST)
            });
            match sent {
                Some(request) => {
                    sim.next_sync = None;
                    self.in_flight.push(InFlight {
                        sim: id,
                        route: Route::Rpc,
                        request,
                        work: Work::Sync,
                    });
                }
                None => {
                    sim.next_sync = Some(Simulator::report_sync(&sim.client, id, None, &self.done))
                }
            }
        }
    }

    /// Delivers the answers that arrived on one endpoint, resends on a hydration miss, and
    /// fails what can no longer be answered.
    fn drive(&mut self, id: usize, route: Route) {
        let sim = &mut self.sims[id];
        let endpoint = match route {
            Route::Rpc => &mut sim.rpc,
            Route::Ssz => &mut sim.ssz,
        };
        let Some(endpoint) = endpoint else { return };
        let pool = endpoint.pool;
        let connected = &mut endpoint.connected;
        let in_flight = &mut self.in_flight;
        let done = &self.done;
        let client = &sim.client;
        let next_sync = &mut sim.next_sync;

        endpoint.http.drive(&mut self.net, |event| {
            match event {
                HttpEvent::Connected { token } if !connected.contains(&token) => {
                    connected.push(token)
                }
                HttpEvent::Disconnected { token } => connected.retain(|live| *live != token),
                _ => {}
            }
            let Some((request, outcome)) = event.outcome(pool) else { return };
            // An answer to a request already failed below has no entry.
            let Some(i) = in_flight.iter().position(|entry| {
                entry.sim == id && entry.route == route && entry.request == request
            }) else {
                return;
            };
            if let Err(reason) = outcome {
                error!(?reason, endpoint = client.endpoint(), "simulator request failed");
            }
            let entry = in_flight.swap_remove(i);
            match entry.work {
                Work::Sync => {
                    let reported = outcome
                        .ok()
                        .and_then(|res| SimulatorClient::sync_outcome(res.status, res.body));
                    *next_sync = Some(Simulator::report_sync(client, id, reported, done));
                }
                Work::Validate(mut validation) => {
                    let res = validation.outcome(route, outcome, client.endpoint());
                    if matches!(res, Err(BlockSimError::HydrationMiss)) && !validation.retried {
                        // On cache miss, retry with full uncompressed SSZ so the simulator
                        // can process the submission without a hydration cache entry.
                        debug!(
                            block_hash = %validation.req.submission.block_hash(),
                            "hydration miss — retrying with full SSZ"
                        );
                        validation.retried = true;
                        validation.resend = true;
                        in_flight.push(InFlight { work: Work::Validate(validation), ..entry });
                    } else {
                        validation.finish(id, res, done);
                    }
                }
                Work::Merged(merged) => {
                    let res = merged.outcome(route, outcome, client.endpoint());
                    merged.finish(id, res, done);
                }
            }
        });

        for entry in self.in_flight.iter_mut() {
            let Work::Validate(validation) = &mut entry.work else { continue };
            if entry.sim != id || entry.route != route || !validation.resend {
                continue;
            }
            // A refused send leaves `resend` set, and the sweep below fails the entry.
            if let Some(request) = endpoint.send(
                &mut self.net,
                &sim.validate_path,
                OCTET_STREAM,
                validation.req.is_top_bid,
                &validation.body,
            ) {
                entry.request = request;
                validation.resend = false;
            }
        }

        // With no connection left, whatever is still here is waiting in the Flux queue with
        // no deadline. Flux may still send it after a reconnect; that answer finds no entry.
        let down = endpoint.connected.is_empty();
        while let Some(i) = self.in_flight.iter().position(|entry| {
            entry.sim == id &&
                entry.route == route &&
                (down || matches!(&entry.work, Work::Validate(v) if v.resend))
        }) {
            error!(endpoint = sim.client.endpoint(), "simulator unreachable");
            match self.in_flight.swap_remove(i).work {
                Work::Sync => {
                    sim.next_sync = Some(Simulator::report_sync(&sim.client, id, None, &self.done))
                }
                Work::Validate(validation) => {
                    validation.finish(id, Err(BlockSimError::RpcError), &self.done)
                }
                Work::Merged(merged) => merged.finish(id, Err(BlockSimError::RpcError), &self.done),
            }
        }
    }
}

impl Tile<HelixSpine> for SimulatorTile {
    fn loop_body(&mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        if self.poll() {
            adapter.mark_work();
        }
    }
}

impl Simulator {
    /// Publishes one sync poll's answer and returns when to poll next. Takes the client
    /// rather than `&mut self` because the answer handler holds an endpoint borrowed.
    fn report_sync(
        client: &SimulatorClient,
        id: usize,
        reported: Option<bool>,
        done: &crossbeam_channel::Sender<SimulatorsEvent>,
    ) -> Instant {
        if done.send(SimulatorsEvent::SyncStatus { id, reported }).is_err() {
            error!("failed to send sync status to sim pool");
        }
        SimulatorMetrics::simulator_sync(client.endpoint(), reported.unwrap_or(false));
        Instant::now() + SYNC_POLL_INTERVAL
    }
}

impl Endpoint {
    /// `None`, after logging why, for a URL this tile cannot reach. The address is
    /// resolved here once: the lookup blocks, and the tile's loop must not.
    fn open(net: &mut Network, raw: &str, connections: usize) -> Option<Self> {
        let url = Url::parse(raw)
            .inspect_err(|err| error!(%err, url = raw, "invalid simulator url"))
            .ok()?;
        let addr = HttpClient::resolve(&url)
            .inspect_err(|err| error!(%err, url = raw, "failed to resolve simulator"))
            .ok()?;
        let name = url.host_str()?;
        let host = match url.port() {
            Some(port) => format!("{name}:{port}"),
            None => name.to_owned(),
        };
        let mut http = HttpNetwork::default()
            .with_name("simulator")
            .with_max_body_bytes(MAX_BODY_BYTES)
            .with_request_timeout(SIMULATOR_REQUEST_TIMEOUT.into())
            .with_reconnect_interval(RECONNECT_INTERVAL.into());
        let pool = match url.scheme() {
            "http" => http.pool(net, addr, connections),
            "https" => http.pool_tls(net, addr, name, connections),
            scheme => {
                error!(scheme, url = raw, "unsupported simulator url scheme");
                return None;
            }
        };
        Some(Self { http, pool, connected: Vec::with_capacity(connections), host })
    }

    /// The request target for `raw`; empty when it is no URL, which
    /// [`Self::open`] reports.
    fn path(raw: &str) -> String {
        Url::parse(raw)
            .map(|url| url[Position::BeforePath..Position::AfterQuery].to_owned())
            .unwrap_or_default()
    }

    fn send(
        &mut self,
        net: &mut Network,
        path: &str,
        content_type: &str,
        high_priority: bool,
        body: &[u8],
    ) -> Option<RequestId> {
        if self.connected.is_empty() {
            return None;
        }
        let headers = [
            ("Host", self.host.as_str()),
            ("Content-Type", content_type),
            ("X-High-Priority", "true"),
        ];
        let headers = if high_priority { &headers[..] } else { &headers[..2] };
        self.http.send(net, self.pool, "POST", path, headers, body, 0)
    }
}

impl Validation {
    fn outcome(
        &self,
        route: Route,
        outcome: Outcome<'_, '_>,
        endpoint: &str,
    ) -> Result<Vec<TxDetail>, BlockSimError> {
        let res = outcome.map_err(|_| BlockSimError::RpcError)?;
        match route {
            Route::Ssz => {
                SimulatorClient::ssz_sim_outcome(res.status, res.body, endpoint, self.body.len())
            }
            Route::Rpc => {
                SimulatorClient::json_sim_outcome(res.status, res.body).map(|()| Vec::new())
            }
        }
    }

    fn finish(
        self,
        id: usize,
        mut res: Result<Vec<TxDetail>, BlockSimError>,
        done: &crossbeam_channel::Sender<SimulatorsEvent>,
    ) {
        let req = self.req;
        let block_hash = *req.submission.block_hash();
        let time = self.timer.stop_and_record();
        debug!(%block_hash, time_secs = time, ?res, "simulation completed");

        SimulatorMetrics::sim_status(res.is_ok());

        if let (Some(got), SignedBidSubmission::Fulu(submission)) = (req.tx_root, &req.submission) {
            let expected = submission.transactions_root();
            if expected != got {
                res = Err(BlockSimError::InvalidTxRoot { got, expected })
            }
        }

        record_submission_step("simulation", self.started.elapsed());

        let error = res.as_ref().err().cloned();
        let bid = Bid::new(req.version, &req.submission, req.parent_beacon_block_root);
        SimulatorMetrics::sim_builder_outcome(
            &bid.builder_pubkey.to_string(),
            req.priority.label(),
            sim_outcome(error.as_ref()),
        );
        let inner = SimulationResultInner {
            submission_ref: req.submission_ref,
            optimistic_version: req.optimistic_version(),
            result: match &error {
                None => Ok(req.trace),
                Some(err) => Err(err.clone()),
            },
            bid: Some(bid),
            submission_id: req.submission_id,
            block_hash,
            txs: res.unwrap_or_default(),
            retried: self.retried,
        };

        let _ = done.send(SimulatorsEvent::TaskDone {
            id,
            error,
            result: Box::new(SimResult::Validate((id, Some(inner)))),
            elapsed: Some(Duration::from_secs_f64(time)),
        });
    }
}

impl MergedValidation {
    fn outcome(
        &self,
        route: Route,
        outcome: Outcome<'_, '_>,
        endpoint: &str,
    ) -> Result<Vec<TxDetail>, BlockSimError> {
        let res = outcome.map_err(|_| BlockSimError::RpcError)?;
        match route {
            Route::Ssz => {
                SimulatorClient::ssz_sim_outcome(res.status, res.body, endpoint, self.request_len)
            }
            Route::Rpc => {
                SimulatorClient::json_sim_outcome(res.status, res.body).map(|()| Vec::new())
            }
        }
    }

    fn finish(
        self,
        id: usize,
        res: Result<Vec<TxDetail>, BlockSimError>,
        done: &crossbeam_channel::Sender<SimulatorsEvent>,
    ) {
        let time = self.timer.stop_and_record();
        debug!(block_hash = %self.block_hash, time_secs = time, ?res, "merged block simulation completed");

        SimulatorMetrics::sim_status(res.is_ok());

        record_submission_step("merge_simulation", self.started.elapsed());

        let error = res.as_ref().err().cloned();
        let (result, txs) = match res {
            Ok(txs) => (Ok(()), txs),
            Err(err) => (Err(err), Vec::new()),
        };
        let inner = MergedSimulationResultInner {
            result,
            submission_id: self.submission_id,
            block_hash: self.block_hash,
            txs,
        };
        let _ = done.send(SimulatorsEvent::TaskDone {
            id,
            error,
            result: Box::new(SimResult::ValidateMerged((id, Some(inner)))),
            elapsed: Some(Duration::from_secs_f64(time)),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use helix_types::{SignedBidSubmissionFulu, SubmissionVersion, TestRandomSeed};

    use super::*;
    use crate::simulator::SimPriority;

    /// Serves `answer(path, nth request for that server)` on a fresh local port, and
    /// answers every sync poll with "synced".
    fn mock_simulator(answer: fn(&str, usize) -> (u16, &'static str)) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let served = served.clone();
                std::thread::spawn(move || serve(stream.unwrap(), answer, &served));
            }
        });
        url
    }

    fn serve(
        mut stream: std::net::TcpStream,
        answer: fn(&str, usize) -> (u16, &'static str),
        served: &AtomicUsize,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if let Some(head) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|at| at + 4) {
                let text = String::from_utf8_lossy(&buf[..head]).to_ascii_lowercase();
                let len = text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                    .unwrap_or(0usize);
                if buf.len() >= head + len {
                    let path = text.split(' ').nth(1).unwrap().to_owned();
                    let syncing = buf[head..head + len] == *SYNC_REQUEST;
                    let (status, body) = if syncing {
                        (200, r#"{"jsonrpc":"2.0","id":"1","result":false}"#)
                    } else {
                        answer(&path, served.fetch_add(1, Ordering::Relaxed))
                    };
                    buf.drain(..head + len);
                    let response = format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    continue;
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buf.extend_from_slice(&chunk[..read]),
            }
        }
    }

    fn unused_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    fn tile(
        url: String,
        ssz_url: Option<String>,
    ) -> (
        SimulatorTile,
        crossbeam_channel::Sender<SimJob>,
        crossbeam_channel::Receiver<SimulatorsEvent>,
    ) {
        let (jobs, jobs_rx) = crossbeam_channel::unbounded();
        let (done, done_rx) = crossbeam_channel::unbounded();
        let config =
            SimulatorConfig { url, namespace: "relay".into(), max_concurrent_tasks: 2, ssz_url };
        (SimulatorTile::new(&[config], jobs_rx, done), jobs, done_rx)
    }

    fn validation() -> SimJob {
        let mut submission = SignedBidSubmissionFulu::test_random();
        submission.blobs_bundle = Default::default();
        let req = ValidationRequest {
            submission_id: Uuid::nil(),
            priority: SimPriority::Top,
            is_top_bid: true,
            is_optimistic: false,
            apply_blacklist: false,
            registered_gas_limit: 0,
            parent_beacon_block_root: B256::ZERO,
            inclusion_list: Default::default(),
            submission: SignedBidSubmission::Fulu(submission),
            tx_root: None,
            version: SubmissionVersion::new(0, None),
            trace: Default::default(),
            receive_ns: 0,
            submission_ref: Default::default(),
        };
        SimJob::Validate { id: 0, req, timer: SimulatorMetrics::timer("mock") }
    }

    /// Turns the tile until `pick` accepts one of its events.
    fn turn_until<T>(
        tile: &mut SimulatorTile,
        done: &crossbeam_channel::Receiver<SimulatorsEvent>,
        mut pick: impl FnMut(SimulatorsEvent) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tile.poll();
            if let Some(picked) = done.try_iter().find_map(&mut pick) {
                return picked;
            }
            assert!(Instant::now() < deadline, "the tile reported nothing in time");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn validated(event: SimulatorsEvent) -> Option<SimulationResultInner> {
        let SimulatorsEvent::TaskDone { result, .. } = event else { return None };
        match *result {
            SimResult::Validate((_, inner)) => inner,
            SimResult::ValidateMerged(_) => None,
        }
    }

    fn synced(event: SimulatorsEvent) -> Option<bool> {
        match event {
            SimulatorsEvent::SyncStatus { reported: Some(synced), .. } => Some(synced),
            _ => None,
        }
    }

    #[test]
    fn a_hydration_miss_is_resent_once() {
        let ssz = mock_simulator(|path, nth| {
            assert_eq!(path, "/validate");
            if nth == 0 { (424, "") } else { (200, "") }
        });
        let (mut tile, jobs, done) = tile(mock_simulator(|_, _| (500, "")), Some(ssz));
        assert!(turn_until(&mut tile, &done, synced));
        while tile.sims[0].ssz.as_ref().unwrap().connected.is_empty() {
            tile.poll();
        }

        jobs.send(validation()).unwrap();
        let result = turn_until(&mut tile, &done, validated);

        assert!(result.retried);
        assert!(result.result.is_ok());
    }

    #[test]
    fn a_second_hydration_miss_stands() {
        let ssz = mock_simulator(|_, _| (424, ""));
        let (mut tile, jobs, done) = tile(mock_simulator(|_, _| (500, "")), Some(ssz));
        assert!(turn_until(&mut tile, &done, synced));
        while tile.sims[0].ssz.as_ref().unwrap().connected.is_empty() {
            tile.poll();
        }

        jobs.send(validation()).unwrap();
        let result = turn_until(&mut tile, &done, validated);

        assert!(result.retried);
        assert!(matches!(result.result, Err(BlockSimError::HydrationMiss)));
    }

    #[test]
    fn a_json_rpc_error_fails_the_block() {
        let rpc = mock_simulator(|path, _| {
            assert_eq!(path, "/");
            (200, r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32000,"message":"bad block"}}"#)
        });
        let (mut tile, jobs, done) = tile(rpc, None);
        assert!(turn_until(&mut tile, &done, synced));

        jobs.send(validation()).unwrap();
        let result = turn_until(&mut tile, &done, validated);

        assert!(matches!(
            result.result,
            Err(BlockSimError::BlockValidationFailed(reason)) if reason == "bad block"
        ));
    }

    /// Flux would hold the request until the simulator came back; the builder must not wait.
    #[test]
    fn a_simulator_that_is_down_is_answered_at_once() {
        let (mut tile, jobs, done) = tile(unused_url(), None);

        jobs.send(validation()).unwrap();
        let result = turn_until(&mut tile, &done, validated);

        assert!(matches!(result.result, Err(BlockSimError::RpcError)));
    }
}
