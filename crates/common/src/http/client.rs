use std::{
    fmt,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use eventsource_stream::Event;
use http_body_util::Full;
use hyper::{Request, body::Bytes, client::conn::http1};
use mio::{Events, Interest, Poll as MioPoll, Token, net::TcpStream};
use rustls::{ClientConfig, ClientConnection, RootCertStore, pki_types::ServerName};
use url::{Position, Url};

pub use crate::http::pending_response::PendingResponse;
use crate::http::{
    error::HttpClientError,
    pending_response::{State, noop_waker},
    sse::{SseRequest, SseState},
    transport::{TlsStream, Transport},
};

const CONN: Token = Token(0);

/// SSE streams are polled from busy-spinning tiles, so without a delay a failing stream
/// reconnects (and re-resolves DNS) on every loop iteration, flooding the node it is trying to
/// reach. After an immediate first retry the delay doubles from the base up to the max, so a
/// recovered node is picked up within a second, well inside a slot.
const SSE_RECONNECT_BASE_DELAY: Duration = Duration::from_millis(50);
const SSE_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(1);
const SSE_FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct HttpClient {
    tls_config: Arc<ClientConfig>,
}

impl HttpClient {
    pub fn new() -> Result<Self, HttpClientError> {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let tls_config =
            Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
        Ok(Self { tls_config })
    }

    /// Blocking DNS lookup. Callers on a hot loop resolve once up front and use `post_to`.
    pub fn resolve(url: &Url) -> Result<SocketAddr, HttpClientError> {
        let host = url.host_str().ok_or(HttpClientError::MissingHost)?;
        let port = url.port_or_known_default().ok_or(HttpClientError::MissingPort)?;
        (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| HttpClientError::DnsError(host.to_string()))
    }

    fn connect(&self, url: &Url) -> Result<(Transport, MioPoll), HttpClientError> {
        self.connect_to(url, Self::resolve(url)?)
    }

    fn connect_to(
        &self,
        url: &Url,
        addr: SocketAddr,
    ) -> Result<(Transport, MioPoll), HttpClientError> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            s => return Err(HttpClientError::UnsupportedScheme(s.to_string())),
        };
        let host = url.host_str().ok_or(HttpClientError::MissingHost)?;
        let mut tcp = TcpStream::connect(addr)?;
        let mio = MioPoll::new()?;
        mio.registry().register(&mut tcp, CONN, Interest::READABLE | Interest::WRITABLE)?;
        let transport = if https {
            let tls = ClientConnection::new(
                self.tls_config.clone(),
                ServerName::try_from(host)?.to_owned(),
            )?;
            Transport::Tls(TlsStream { tcp, tls })
        } else {
            Transport::Plain(tcp)
        };
        Ok((transport, mio))
    }

    /// Connect to `url` and send `req`. The caller is responsible for building the request
    /// (method, path, host header, body). Returns a `PendingResponse` that must be polled.
    pub fn send(
        &self,
        url: &Url,
        req: Request<Full<Bytes>>,
    ) -> Result<PendingResponse, HttpClientError> {
        let (transport, mio) = self.connect(url)?;
        Ok(Self::pending(transport, mio, req))
    }

    fn pending(transport: Transport, mio: MioPoll, req: Request<Full<Bytes>>) -> PendingResponse {
        let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
        PendingResponse::new(State::Handshaking { handshake, req }, mio, Events::with_capacity(8))
    }

    pub fn get(&self, url: &Url) -> Result<PendingResponse, HttpClientError> {
        let req = Request::get(&url[Position::BeforePath..Position::AfterQuery])
            .header("host", url.host_str().unwrap_or_default())
            .body(Full::new(Bytes::new()))?;
        self.send(url, req)
    }

    /// `get` asking for an SSZ body, as the beacon API serves on `Accept:
    /// application/octet-stream`.
    pub fn get_ssz(&self, url: &Url) -> Result<PendingResponse, HttpClientError> {
        let req = Request::get(url.path())
            .header("host", url.host_str().unwrap_or_default())
            .header("accept", "application/octet-stream")
            .body(Full::new(Bytes::new()))?;
        self.send(url, req)
    }

    pub fn post(&self, url: &Url, body: Bytes) -> Result<PendingResponse, HttpClientError> {
        self.send(url, Self::post_request(url, body)?)
    }

    /// `post` to an address already resolved with `resolve`, so no DNS lookup blocks.
    pub fn post_to(
        &self,
        url: &Url,
        addr: SocketAddr,
        body: Bytes,
    ) -> Result<PendingResponse, HttpClientError> {
        let (transport, mio) = self.connect_to(url, addr)?;
        Ok(Self::pending(transport, mio, Self::post_request(url, body)?))
    }

    fn post_request(url: &Url, body: Bytes) -> Result<Request<Full<Bytes>>, HttpClientError> {
        let req = Request::post(&url[Position::BeforePath..Position::AfterQuery])
            .header("host", url.host_str().unwrap_or_default())
            .header("content-type", "application/json")
            .header("content-length", body.len().to_string())
            .body(Full::new(body))?;
        Ok(req)
    }

    /// Create a reconnecting SSE stream. Call `poll()` once per loop iteration.
    pub fn sse_stream(&self, url: Url) -> SseStream {
        let now = Instant::now();
        SseStream {
            client: self.clone(),
            url,
            inner: None,
            retry_delay: Duration::ZERO,
            reconnect_at: now,
            next_failure_log: now,
        }
    }
}

/// Reconnecting SSE stream. Call `poll()` once per loop iteration; yields events indefinitely.
pub struct SseStream {
    client: HttpClient,
    url: Url,
    inner: Option<SseRequest>,
    /// Zero until a failure, so the first retry is immediate. Only an event resets it: a node can
    /// accept connections and drop them straight away, which must still back off.
    retry_delay: Duration,
    reconnect_at: Instant,
    next_failure_log: Instant,
}

impl SseStream {
    pub fn poll(&mut self) -> Poll<Event> {
        if self.inner.is_none() {
            if Instant::now() < self.reconnect_at {
                return Poll::Pending;
            }
            let attempt = self.client.connect(&self.url).and_then(|(transport, mio)| {
                let req = Request::get(&self.url[Position::BeforePath..Position::AfterQuery])
                    .header("host", self.url.host_str().unwrap_or_default())
                    .header("accept", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .body(Full::new(Bytes::new()))?;
                let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
                Ok(SseRequest {
                    state: SseState::Handshaking { handshake, req },
                    mio,
                    events: Events::with_capacity(8),
                    waker: noop_waker(),
                })
            });
            match attempt {
                Ok(request) => self.inner = Some(request),
                Err(e) => {
                    self.record_failure(&e);
                    return Poll::Pending;
                }
            }
        }
        match self.inner.as_mut().unwrap().poll() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(event))) => {
                if !self.retry_delay.is_zero() {
                    tracing::info!(url = %self.url, "SSE reconnected");
                    self.retry_delay = Duration::ZERO;
                }
                Poll::Ready(event)
            }
            Poll::Ready(None) => {
                self.record_failure(&"stream ended by server");
                Poll::Pending
            }
            Poll::Ready(Some(Err(e))) => {
                self.record_failure(&e);
                Poll::Pending
            }
        }
    }

    fn record_failure(&mut self, err: &dyn fmt::Display) {
        self.inner = None;
        let now = Instant::now();
        if self.retry_delay.is_zero() || now >= self.next_failure_log {
            tracing::warn!(%err, url = %self.url, "SSE failed, will reconnect");
            self.next_failure_log = now + SSE_FAILURE_LOG_INTERVAL;
        }
        self.reconnect_at = now + self.retry_delay;
        self.retry_delay =
            (self.retry_delay * 2).clamp(SSE_RECONNECT_BASE_DELAY, SSE_RECONNECT_MAX_DELAY);
    }
}

#[test]
fn client_test() -> Result<(), Box<dyn std::error::Error>> {
    use std::{pin::Pin, task::Context};

    crate::utils::install_default_crypto_provider();
    let client = HttpClient::new()?;
    let url = Url::parse("https://ifconfig.me/")?;
    let mut pending = client.get(&url)?;
    let (_status, bytes) = loop {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        if let Poll::Ready(r) = Pin::new(&mut pending).poll(&mut cx) {
            break r?;
        }
    };
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

#[test]
fn sse_stream_backs_off_between_reconnects() {
    use std::{
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    crate::utils::install_default_crypto_provider();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url =
        Url::parse(&format!("http://{}/eth/v1/events", listener.local_addr().unwrap())).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    // Accepts and closes every connection without an event, so connecting succeeds but each
    // attempt still fails and must back off.
    thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    let mut sse = HttpClient::new().unwrap().sse_stream(url);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        let _ = sse.poll();
    }

    // Attempts at ~0, ~0 (immediate retry), 50, 150 and 350ms; the next is due at 750ms.
    let attempts = accepted.load(Ordering::Relaxed);
    assert!((4..=6).contains(&attempts), "{attempts} connection attempts in 500ms");
}
