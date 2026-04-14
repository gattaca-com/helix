use std::{
    future::Future,
    io::{self, Read, Write},
    net::ToSocketAddrs,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    time::Duration,
};

use eventsource_stream::{Event, EventStream, Eventsource};
use futures::stream::Stream;
use http_body_util::{BodyExt, Collected, Full};
use hyper::{
    Request, Response,
    body::{Body, Bytes, Incoming},
    client::conn::http1::{self, Connection, SendRequest},
};
use mio::{Events, Interest, Poll as MioPoll, Token, net::TcpStream};
use rustls::{ClientConfig, ClientConnection, OwnedTrustAnchor, RootCertStore, ServerName};
use url::Url;

const CONN: Token = Token(0);

// Noop waker — we re-poll manually after every mio event, so no wakeup mechanism needed.
unsafe fn noop_clone(p: *const ()) -> RawWaker {
    RawWaker::new(p, &NOOP_VT)
}
unsafe fn noop(_: *const ()) {}
static NOOP_VT: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);
fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VT)) }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type HyperConn = Pin<Box<Connection<Transport, Full<Bytes>>>>;

// Plain TCP or TLS over TCP, unified for hyper's IO traits.
enum Transport {
    Plain(TcpStream),
    Tls(TlsStream),
}

impl hyper::rt::Read for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(tcp) => {
                let cap = buf.remaining();
                if cap == 0 {
                    return Poll::Ready(Ok(()));
                }
                let mut tmp = vec![0u8; cap];
                match tcp.read(&mut tmp) {
                    Ok(0) => Poll::Ready(Ok(())),
                    Ok(n) => {
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                tmp.as_ptr(),
                                buf.as_mut().as_mut_ptr().cast::<u8>(),
                                n,
                            );
                            buf.advance(n);
                        }
                        Poll::Ready(Ok(()))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Transport::Tls(tls) => Pin::new(tls).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Transport::Plain(tcp) => match tcp.write(buf) {
                Ok(n) => Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            },
            Transport::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(tcp) => match tcp.flush() {
                Ok(()) => Poll::Ready(Ok(())),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            },
            Transport::Tls(tls) => Pin::new(tls).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(_) => Poll::Ready(Ok(())),
            Transport::Tls(tls) => Pin::new(tls).poll_shutdown(cx),
        }
    }
}

// Each variant owns the in-progress futures. Pin<Box<T>>: Unpin regardless of T,
// so we can move them out with mem::replace using Done as a sentinel.
enum State {
    Handshaking {
        handshake: BoxFuture<
            hyper::Result<(SendRequest<Full<Bytes>>, Connection<Transport, Full<Bytes>>)>,
        >,
        req: Request<Full<Bytes>>,
    },
    Responding {
        conn: HyperConn,
        resp: BoxFuture<hyper::Result<Response<Incoming>>>,
    },
    Collecting {
        conn: HyperConn,
        body: BoxFuture<Result<Collected<Bytes>, hyper::Error>>,
    },
    Done,
}

pub struct PendingResponse {
    state: State,
    mio: MioPoll,
    events: Events,
    waker: Waker,
}

impl PendingResponse {
    /// Advance the request by one step. Call once per loop iteration.
    /// Returns `Poll::Pending` until complete or `Poll::Ready(Err(...))` on failure.
    pub fn poll(&mut self) -> Poll<Result<Bytes, Box<dyn std::error::Error>>> {
        let mut cx = Context::from_waker(&self.waker);
        // Non-blocking drain of IO events so the hyper futures can make progress.
        let _ = self.mio.poll(&mut self.events, Some(Duration::ZERO));
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::Handshaking { mut handshake, req } => {
                    match handshake.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = State::Handshaking { handshake, req };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok((mut sender, conn))) => {
                            let resp = Box::pin(sender.send_request(req));
                            self.state = State::Responding { conn: Box::pin(conn), resp };
                        }
                    }
                }
                State::Responding { mut conn, mut resp } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(&mut cx) {
                        return Poll::Ready(Err(e.into()));
                    }
                    match resp.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = State::Responding { conn, resp };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok(response)) => {
                            let body = Box::pin(response.into_body().collect());
                            self.state = State::Collecting { conn, body };
                        }
                    }
                }
                State::Collecting { mut conn, mut body } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(&mut cx) {
                        return Poll::Ready(Err(e.into()));
                    }
                    match body.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = State::Collecting { conn, body };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok(collected)) => return Poll::Ready(Ok(collected.to_bytes())),
                    }
                }
                State::Done => panic!("PendingResponse::poll called after completion"),
            }
        }
    }

    /// Convenience wrapper: poll to completion then deserialize the body as JSON.
    pub fn poll_json<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Poll<Result<T, Box<dyn std::error::Error>>> {
        match self.poll() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(bytes)) => {
                Poll::Ready(serde_json::from_slice(&bytes).map_err(|e| Box::new(e) as _))
            }
        }
    }
}

pub struct HttpClient {
    tls_config: Arc<ClientConfig>,
}

impl HttpClient {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut roots = RootCertStore::empty();
        roots.add_trust_anchors(
            webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
                OwnedTrustAnchor::from_subject_spki_name_constraints(
                    ta.subject,
                    ta.spki,
                    ta.name_constraints,
                )
            }),
        );
        let tls_config =
            Arc::new(ClientConfig::builder().with_safe_defaults().with_root_certificates(roots).with_no_client_auth());
        Ok(Self { tls_config })
    }

    fn connect(
        &self,
        url: &Url,
    ) -> Result<(Transport, MioPoll, String, String), Box<dyn std::error::Error>> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            s => return Err(format!("unsupported scheme: {s}").into()),
        };
        let host = url.host_str().ok_or("URL missing host")?.to_string();
        let port = url.port_or_known_default().ok_or("URL missing port")?;
        let path = {
            let p = url.path();
            let q = url.query();
            if let Some(q) = q { format!("{p}?{q}") } else { p.to_string() }
        };

        let addr = (host.as_str(), port).to_socket_addrs()?.next().ok_or("DNS: no address")?;
        let mut tcp = TcpStream::connect(addr)?;
        let mio = MioPoll::new()?;
        mio.registry().register(&mut tcp, CONN, Interest::READABLE | Interest::WRITABLE)?;
        let transport = if https {
            let tls = ClientConnection::new(
                self.tls_config.clone(),
                ServerName::try_from(host.as_str())?.to_owned(),
            )?;
            Transport::Tls(TlsStream { tcp, tls })
        } else {
            Transport::Plain(tcp)
        };
        Ok((transport, mio, host, path))
    }

    /// Returns an `PendingResponse` that must be polled to completion.
    pub fn get(&self, url: &Url) -> Result<PendingResponse, Box<dyn std::error::Error>> {
        let (transport, mio, host, path) = self.connect(url)?;
        let req = Request::get(&path).header("host", &host).body(Full::new(Bytes::new()))?;
        let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
        Ok(PendingResponse {
            state: State::Handshaking { handshake, req },
            mio,
            events: Events::with_capacity(8),
            waker: noop_waker(),
        })
    }

    /// Returns a `PendingResponse` for a POST request with a JSON body.
    pub fn post(
        &self,
        url: &Url,
        body: Bytes,
    ) -> Result<PendingResponse, Box<dyn std::error::Error>> {
        let (transport, mio, host, path) = self.connect(url)?;
        let len = body.len();
        let req = Request::post(&path)
            .header("host", &host)
            .header("content-type", "application/json")
            .header("content-length", len.to_string())
            .body(Full::new(body))?;
        let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
        Ok(PendingResponse {
            state: State::Handshaking { handshake, req },
            mio,
            events: Events::with_capacity(8),
            waker: noop_waker(),
        })
    }

    /// Create a reconnecting SSE stream. Call `poll()` once per loop iteration.
    pub fn sse_stream(&self, url: Url) -> SseStream {
        SseStream { tls_config: self.tls_config.clone(), url, inner: None }
    }
}

// --- SSE ---

// Drives the hyper connection alongside the response body, yielding raw byte chunks.
// Sits underneath EventStream which handles the SSE framing.
struct SseChunkStream {
    conn: HyperConn,
    body: Pin<Box<Incoming>>,
}

impl Stream for SseChunkStream {
    type Item = Result<Bytes, hyper::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _ = this.conn.as_mut().poll(cx);
        match this.body.as_mut().poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => Poll::Ready(Some(Ok(data))),
                Err(_) => Poll::Pending, // trailers
            },
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
        }
    }
}

enum SseState {
    Handshaking {
        handshake: BoxFuture<
            hyper::Result<(SendRequest<Full<Bytes>>, Connection<Transport, Full<Bytes>>)>,
        >,
        req: Request<Full<Bytes>>,
    },
    Responding {
        conn: HyperConn,
        resp: BoxFuture<hyper::Result<Response<Incoming>>>,
    },
    Streaming(Pin<Box<EventStream<SseChunkStream>>>),
    Done,
}

struct SseRequest {
    state: SseState,
    mio: MioPoll,
    events: Events,
    waker: Waker,
}

impl SseRequest {
    /// Advance one step. Returns `None` when the server closes the stream (reconnect needed).
    pub fn poll(&mut self) -> Poll<Option<Result<Event, Box<dyn std::error::Error>>>> {
        let mut cx = Context::from_waker(&self.waker);
        let _ = self.mio.poll(&mut self.events, Some(Duration::ZERO));
        loop {
            match std::mem::replace(&mut self.state, SseState::Done) {
                SseState::Handshaking { mut handshake, req } => {
                    match handshake.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = SseState::Handshaking { handshake, req };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                        Poll::Ready(Ok((mut sender, conn))) => {
                            let resp = Box::pin(sender.send_request(req));
                            self.state = SseState::Responding { conn: Box::pin(conn), resp };
                        }
                    }
                }
                SseState::Responding { mut conn, mut resp } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(&mut cx) {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                    match resp.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = SseState::Responding { conn, resp };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                        Poll::Ready(Ok(response)) => {
                            let body = Box::pin(response.into_body());
                            let stream = Box::pin(SseChunkStream { conn, body }.eventsource());
                            self.state = SseState::Streaming(stream);
                        }
                    }
                }
                SseState::Streaming(mut stream) => match stream.as_mut().poll_next(&mut cx) {
                    Poll::Pending => {
                        self.state = SseState::Streaming(stream);
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some(Ok(event))) => {
                        self.state = SseState::Streaming(stream);
                        return Poll::Ready(Some(Ok(event)));
                    }
                    Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(Box::new(e)))),
                },
                SseState::Done => panic!("SseRequest::poll called after completion"),
            }
        }
    }
}

/// Reconnecting SSE stream. Call `poll()` once per loop iteration; yields events indefinitely.
pub struct SseStream {
    tls_config: Arc<ClientConfig>,
    url: Url,
    inner: Option<SseRequest>,
}

impl SseStream {
    pub fn poll(&mut self) -> Poll<Event> {
        if self.inner.is_none() {
            let client = HttpClient { tls_config: self.tls_config.clone() };
            match client.connect(&self.url) {
                Err(e) => {
                    tracing::warn!(err = %e, url = %self.url, "SSE connect failed");
                    return Poll::Pending;
                }
                Ok((transport, mio, host, path)) => {
                    let req = match Request::get(&path)
                        .header("host", &host)
                        .header("accept", "text/event-stream")
                        .header("cache-control", "no-cache")
                        .body(Full::new(Bytes::new()))
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(err = %e, url = %self.url, "SSE request build failed");
                            return Poll::Pending;
                        }
                    };
                    let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
                    self.inner = Some(SseRequest {
                        state: SseState::Handshaking { handshake, req },
                        mio,
                        events: Events::with_capacity(8),
                        waker: noop_waker(),
                    });
                }
            }
        }
        match self.inner.as_mut().unwrap().poll() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                tracing::warn!(url = %self.url, "SSE stream ended, will reconnect");
                self.inner = None;
                Poll::Pending
            }
            Poll::Ready(Some(Ok(event))) => Poll::Ready(event),
            Poll::Ready(Some(Err(e))) => {
                tracing::warn!(err = %e, url = %self.url, "SSE error, will reconnect");
                self.inner = None;
                Poll::Pending
            }
        }
    }
}

struct TlsStream {
    tcp: TcpStream,
    tls: ClientConnection,
}

impl TlsStream {
    fn flush_tls(&mut self) -> io::Result<()> {
        while self.tls.wants_write() {
            match self.tls.write_tls(&mut self.tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl hyper::rt::Read for TlsStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Pump encrypted bytes from TCP into rustls.
        loop {
            match this.tls.read_tls(&mut this.tcp) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        if let Err(e) = this.tls.process_new_packets() {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
        }
        let cap = buf.remaining();
        if cap == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut tmp = vec![0u8; cap];
        match this.tls.reader().read(&mut tmp) {
            Ok(0) => Poll::Ready(Ok(())),
            Ok(n) => {
                // SAFETY: copy_nonoverlapping initialises the n bytes we advance past.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        tmp.as_ptr(),
                        buf.as_mut().as_mut_ptr().cast::<u8>(),
                        n,
                    );
                    buf.advance(n);
                }
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl hyper::rt::Write for TlsStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = match this.tls.writer().write(buf) {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(e)),
        };
        Poll::Ready(this.flush_tls().map(|_| n))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().flush_tls())
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn client_test() -> Result<(), Box<dyn std::error::Error>> {
    let client = HttpClient::new()?;
    let url = Url::parse("https://ifconfig.me/")?;
    let mut req = client.get(&url)?;
    let bytes = loop {
        if let Poll::Ready(r) = req.poll() {
            break r?;
        }
    };
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}
