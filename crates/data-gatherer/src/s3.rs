use std::{
    collections::VecDeque,
    net::{SocketAddr, ToSocketAddrs},
    time::{Duration, Instant},
};

use flux_network::{NetworkCore, NetworkEvent};
use flux_s3::{Error, RequestId, S3};
use helix_common::{
    S3Config, api::builder_api::MAX_PAYLOAD_LENGTH, expect_env_var, utils::utcnow_ms,
};
use helix_relay::InternalBidSubmissionHeader;
use rustc_hash::FxHashMap;

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

/// Uploads in flight at once; each connection carries one request at a time.
const CONNECTIONS: usize = 8;
const BATCH_BYTES: usize = 64 * 1024 * 1024;
/// A full batch overshoots by at most one submission plus its header.
const MAX_BATCH_BYTES: usize = BATCH_BYTES + MAX_PAYLOAD_LENGTH + 4096;
/// flux caps a connection's backlog at this plus a fixed 64 KiB, counting TLS
/// ciphertext; record overhead (~0.14%) exceeds that margin at batch size.
const MAX_BODY_BYTES: usize = MAX_BATCH_BYTES + MAX_BATCH_BYTES / 100;
/// What a stalled bucket may hold in memory before further uploads are shed.
const MAX_QUEUED_BYTES: usize = 512 * 1024 * 1024;
/// Total sends per object.
const MAX_ATTEMPTS: u8 = 5;
/// First retry delay; doubles per attempt (0.5/1/2/4 s).
const RETRY_BASE: Duration = Duration::from_millis(500);
/// Retained retry bodies past this shed the oldest first.
const MAX_RETRY_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Least gap between endpoint lookups triggered by connection failures.
const RERESOLVE_INTERVAL: Duration = Duration::from_secs(10);

/// Records are `[u32 LE len][u16 LE header_len][header][payload]`, `len`
/// covering what follows it.
#[derive(Default)]
struct Batch {
    slot: u64,
    records: u32,
    body: Vec<u8>,
}

impl Batch {
    fn push(&mut self, slot: u64, header: &[u8], payload: &[u8]) {
        if self.records == 0 {
            self.slot = slot;
            self.body.reserve(BATCH_BYTES);
        }
        let len = 2 + header.len() + payload.len();
        self.body.extend_from_slice(&(len as u32).to_le_bytes());
        self.body.extend_from_slice(&(header.len() as u16).to_le_bytes());
        self.body.extend_from_slice(header);
        self.body.extend_from_slice(payload);
        self.records += 1;
    }
}

struct Upload {
    key: String,
    body: Vec<u8>,
    records: u32,
    /// Sends so far.
    attempts: u8,
}

struct RetryUpload {
    up: Upload,
    not_before: Instant,
}

pub struct S3Data {
    s3: S3,
    config: S3Config,
    /// Address the client is connected to; the endpoint's DNS rotates.
    addr: SocketAddr,
    resolved_at: Instant,
    batch: Batch,
    instance: String,
    /// Keeps keys unique across restarts within one slot.
    started_ms: u64,
    seq: u64,
    /// Keyed by request so a failure can name the object it lost. The body
    /// is kept for a retryable failure.
    in_flight: FxHashMap<RequestId, Upload>,
    retry: VecDeque<RetryUpload>,
    retry_bytes: usize,
    failures: u32,
}

impl S3Data {
    pub fn new(config: &S3Config, instance: &str, net: &mut NetworkCore) -> Self {
        let addr = Self::resolve(&config.endpoint).unwrap_or_else(|| {
            panic!("s3 endpoint `{}` is not a host:port with an IPv4 address", config.endpoint)
        });
        let mut s3 = Self::client(config, addr);
        s3.connect(net);

        Self {
            s3,
            config: config.clone(),
            addr,
            resolved_at: Instant::now(),
            batch: Batch::default(),
            instance: instance.to_owned(),
            started_ms: utcnow_ms(),
            seq: 0,
            in_flight: FxHashMap::default(),
            retry: VecDeque::new(),
            retry_bytes: 0,
            failures: 0,
        }
    }

    fn client(config: &S3Config, addr: SocketAddr) -> S3 {
        let access_key_id = expect_env_var(ENV_ACCESS_KEY_ID);
        let secret_access_key = expect_env_var(ENV_SECRET_ACCESS_KEY);
        let host = config.endpoint.rsplit_once(':').map_or(config.endpoint.as_str(), |(h, _)| h);
        if config.tls { S3::new_tls(addr, host, CONNECTIONS) } else { S3::new(addr, CONNECTIONS) }
            .with_region(&config.region)
            .with_credentials(&access_key_id, &secret_access_key)
            .with_http(|http| {
                http.with_max_body_bytes(MAX_BODY_BYTES)
                    .with_max_queued_bytes(MAX_QUEUED_BYTES)
                    .with_request_timeout(REQUEST_TIMEOUT.into())
            })
    }

    fn resolve(endpoint: &str) -> Option<SocketAddr> {
        endpoint.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)
    }

    /// Moves to the endpoint's current address after connection failures.
    /// Closing the old client drops its in-flight requests without outcomes,
    /// so their retained bodies are queued for another attempt.
    fn follow_endpoint(&mut self, net: &mut NetworkCore) {
        if self.resolved_at.elapsed() < RERESOLVE_INTERVAL {
            return;
        }
        self.resolved_at = Instant::now();
        let Some(addr) = Self::resolve(&self.config.endpoint) else {
            tracing::warn!(endpoint = %self.config.endpoint, "s3 endpoint did not resolve");
            return;
        };
        if addr == self.addr {
            return;
        }
        tracing::info!(old = %self.addr, new = %addr, "s3 endpoint address changed, reconnecting");
        self.addr = addr;
        let mut s3 = Self::client(&self.config, addr);
        s3.connect(net);
        std::mem::replace(&mut self.s3, s3).close(net);
        for (_, up) in std::mem::take(&mut self.in_flight) {
            self.requeue(up);
        }
    }

    /// Failures since the last call. Drained once per slot by the stats log.
    pub fn take_failures(&mut self) -> u32 {
        std::mem::take(&mut self.failures)
    }

    /// Uploads still awaiting an outcome, plus retries still awaiting a send.
    pub fn pending(&self) -> usize {
        self.in_flight.len() + self.retry.len()
    }

    pub fn upload(
        &mut self,
        net: &mut NetworkCore,
        slot: u64,
        header: InternalBidSubmissionHeader,
        payload: &[u8],
    ) {
        if self.batch.records > 0 && self.batch.slot != slot {
            self.flush(net);
        }
        self.batch.push(slot, header.to_bytes().as_slice(), payload);
        if self.batch.body.len() >= BATCH_BYTES {
            self.flush(net);
        }
    }

    pub fn flush(&mut self, net: &mut NetworkCore) {
        if self.batch.records == 0 {
            return;
        }
        let Batch { slot, records, body } = std::mem::take(&mut self.batch);
        let key =
            format!("batches/{slot}/{}/{}-{:06}.bin", self.instance, self.started_ms, self.seq);
        self.seq += 1;
        self.send(net, Upload { key, body, records, attempts: 0 });
    }

    /// Returns false when the network refuses the upload unsent.
    fn send(&mut self, net: &mut NetworkCore, up: Upload) -> bool {
        match self.s3.put_object(net, &self.config.bucket, &up.key, &up.body) {
            Some(id) => {
                self.in_flight.insert(id, Upload { attempts: up.attempts + 1, ..up });
                true
            }
            // Never sent: park for retry like any other backpressure.
            None => {
                self.requeue(up);
                false
            }
        }
    }

    /// Parks a failed upload for another attempt, or counts it lost. Shared
    /// with the refused-before-send path, which also never reached the wire.
    fn requeue(&mut self, up: Upload) {
        if up.attempts >= MAX_ATTEMPTS || self.retry_bytes + up.body.len() > MAX_RETRY_BYTES {
            if self.failures == 0 {
                tracing::error!(key = %up.key, records = up.records, "s3 upload lost");
            }
            self.failures += up.records;
            return;
        }
        let backoff = RETRY_BASE * 2u32.pow(up.attempts as u32);
        self.retry_bytes += up.body.len();
        self.retry.push_back(RetryUpload { up, not_before: Instant::now() + backoff });
    }

    fn is_retryable(err: &Error) -> bool {
        match err {
            Error::Server { status: 429 | 500 | 503, .. } => true,
            Error::Server { .. } => false,
            Error::Disconnected | Error::TimedOut => true,
        }
    }

    /// Sends due retries until one is refused. Backoff grows with each
    /// attempt, so the queue is not ordered by `not_before`: any entry may be due.
    fn pump(&mut self, net: &mut NetworkCore) {
        let now = Instant::now();
        while let Some(pos) = self.retry.iter().position(|r| r.not_before <= now) {
            let RetryUpload { up, .. } = self.retry.remove(pos).expect("position is in bounds");
            self.retry_bytes -= up.body.len();
            if !self.send(net, up) {
                break;
            }
        }
    }

    /// Returns whether the event belonged to this client.
    pub fn on_event(&mut self, event: &NetworkEvent<'_>) -> bool {
        self.s3.on_event(event)
    }

    /// Sends due retries, then what is queued, and retires finished uploads.
    pub fn drive(&mut self, net: &mut NetworkCore) -> bool {
        self.pump(net);
        // Staged out of the callback: `put_object` cannot run while `drive`
        // holds the client.
        let mut requeues = Vec::new();
        let mut worked = false;
        let mut unreachable = false;
        {
            let Self { s3, in_flight, failures, .. } = self;
            s3.drive(net, |id, result| {
                worked = true;
                let Some(up) = in_flight.remove(&id) else { return };
                let Err(err) = result else { return };
                unreachable |= matches!(err, Error::Disconnected | Error::TimedOut);
                if Self::is_retryable(&err) && up.attempts < MAX_ATTEMPTS {
                    requeues.push(up);
                    return;
                }
                if *failures == 0 {
                    match err {
                        Error::Server { status, code, message } => tracing::error!(
                            status,
                            code = code.unwrap_or("none"),
                            detail = %String::from_utf8_lossy(message),
                            key = %up.key,
                            "s3 upload failed"
                        ),
                        other => tracing::error!(?other, key = %up.key, "s3 upload failed"),
                    }
                }
                *failures += up.records;
            });
        }
        for up in requeues {
            self.requeue(up);
        }
        if unreachable {
            self.follow_endpoint(net);
        }
        worked
    }
}
