use std::{
    collections::VecDeque,
    net::{SocketAddr, ToSocketAddrs},
    time::{Duration, Instant},
};

use flux_network::tcp::{TcpEvent, TcpNetworkCore};
use flux_s3::{Error, RequestId, S3};
use helix_common::{S3Config, api::builder_api::MAX_PAYLOAD_LENGTH, expect_env_var};
use helix_relay::InternalBidSubmissionHeader;
use rustc_hash::FxHashMap;

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

/// Uploads in flight at once; each connection carries one request at a time.
const CONNECTIONS: usize = 8;
/// A submission plus its serialised header.
const MAX_BODY_BYTES: usize = MAX_PAYLOAD_LENGTH + 4096;
/// What a stalled bucket may hold in memory before further uploads are shed.
const MAX_QUEUED_BYTES: usize = 512 * 1024 * 1024;
/// Total sends per object, matching the old adaptive client.
const MAX_ATTEMPTS: u8 = 3;
/// First retry delay; doubles per attempt (100/200/400 ms).
const RETRY_BASE: Duration = Duration::from_millis(100);
/// Retained retry bodies past this shed the oldest first.
const MAX_RETRY_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Least gap between endpoint lookups triggered by connection failures.
const RERESOLVE_INTERVAL: Duration = Duration::from_secs(10);

struct InflightUpload {
    key: String,
    body: Vec<u8>,
    /// Sends so far.
    attempts: u8,
}

struct RetryUpload {
    key: String,
    body: Vec<u8>,
    attempts: u8,
    not_before: Instant,
}

pub struct S3Data {
    s3: S3,
    config: S3Config,
    /// Address the client is connected to; the endpoint's DNS rotates.
    addr: SocketAddr,
    resolved_at: Instant,
    /// Keyed by request so a failure can name the object it lost. The body
    /// is kept for a retryable failure.
    in_flight: FxHashMap<RequestId, InflightUpload>,
    retry: VecDeque<RetryUpload>,
    retry_bytes: usize,
    failures: u32,
}

impl S3Data {
    pub fn new(config: &S3Config, net: &mut TcpNetworkCore) -> Self {
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
    fn follow_endpoint(&mut self, net: &mut TcpNetworkCore) {
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
            self.requeue(up.key, up.body, up.attempts);
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

    pub fn upload(&mut self, header: InternalBidSubmissionHeader, payload: &[u8]) {
        let key = format!("{}.bin", header.submission_id);
        let header = header.to_bytes();
        let header = header.as_slice();
        let header_len = header.len() as u16;

        // format: [u16 LE header_len][header bytes][payload bytes]
        let mut body = Vec::with_capacity(2 + header.len() + payload.len());
        body.extend_from_slice(&header_len.to_le_bytes());
        body.extend_from_slice(header);
        body.extend_from_slice(payload);

        match self.s3.put_object(&self.config.bucket, &key, body.clone()) {
            Ok(id) => {
                self.in_flight.insert(id, InflightUpload { key, body, attempts: 1 });
            }
            // Never sent: park for retry like any other backpressure.
            Err(_) => self.requeue(key, body, 0),
        }
    }

    /// Parks a failed upload for another attempt, or counts it lost. Shared
    /// with the refused-before-send path, which also never reached the wire.
    fn requeue(&mut self, key: String, body: Vec<u8>, attempts: u8) {
        if attempts >= MAX_ATTEMPTS || self.retry_bytes + body.len() > MAX_RETRY_BYTES {
            if self.failures == 0 {
                tracing::error!(%key, "s3 upload lost");
            }
            self.failures += 1;
            return;
        }
        let backoff = RETRY_BASE * 2u32.pow(attempts as u32);
        self.retry_bytes += body.len();
        self.retry.push_back(RetryUpload {
            key,
            body,
            attempts,
            not_before: Instant::now() + backoff,
        });
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
    fn pump(&mut self) {
        let now = Instant::now();
        while let Some(pos) = self.retry.iter().position(|up| up.not_before <= now) {
            let up = self.retry.remove(pos).expect("position is in bounds");
            self.retry_bytes -= up.body.len();
            match self.s3.put_object(&self.config.bucket, &up.key, up.body.clone()) {
                Ok(id) => {
                    self.in_flight.insert(id, InflightUpload {
                        key: up.key,
                        body: up.body,
                        attempts: up.attempts + 1,
                    });
                }
                Err(_) => {
                    self.requeue(up.key, up.body, up.attempts);
                    break;
                }
            }
        }
    }

    /// Returns whether the event belonged to this client.
    pub fn on_event(&mut self, event: &TcpEvent<'_>) -> bool {
        self.s3.on_event(event)
    }

    /// Sends due retries, then what is queued, and retires finished uploads.
    pub fn drive(&mut self, net: &mut TcpNetworkCore) -> bool {
        self.pump();
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
                    requeues.push((up.key, up.body, up.attempts));
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
                *failures += 1;
            });
        }
        for (key, body, attempts) in requeues {
            self.requeue(key, body, attempts);
        }
        if unreachable {
            self.follow_endpoint(net);
        }
        worked
    }
}
