use std::{net::ToSocketAddrs, sync::Arc, task::Poll};

use eventsource_stream::Event;
use http_body_util::Full;
use hyper::{Request, body::Bytes, client::conn::http1};
use mio::{Events, Interest, Poll as MioPoll, Token, net::TcpStream};
use rustls::{ClientConfig, ClientConnection, OwnedTrustAnchor, RootCertStore, ServerName};
use url::Url;

pub use crate::http::pending_response::PendingResponse;
use crate::http::{
    error::HttpClientError,
    pending_response::{State, noop_waker},
    sse::{SseRequest, SseState},
    transport::{TlsStream, Transport},
};

const CONN: Token = Token(0);

#[derive(Clone, Debug)]
pub struct HttpClient {
    tls_config: Arc<ClientConfig>,
}

impl HttpClient {
    pub fn new() -> Result<Self, HttpClientError> {
        let mut roots = RootCertStore::empty();
        roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        let tls_config = Arc::new(
            ClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        Ok(Self { tls_config })
    }

    fn connect(&self, url: &Url) -> Result<(Transport, MioPoll), HttpClientError> {
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            s => return Err(HttpClientError::UnsupportedScheme(s.to_string())),
        };
        let host = url.host_str().unwrap();
        let port = url.port_or_known_default().unwrap();

        let addr = (host, port).to_socket_addrs()?.next().unwrap();
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
        let handshake = Box::pin(http1::handshake::<_, Full<Bytes>>(transport));
        Ok(PendingResponse {
            state: State::Handshaking { handshake, req },
            mio,
            events: Events::with_capacity(8),
        })
    }

    pub fn get(&self, url: &Url) -> Result<PendingResponse, HttpClientError> {
        let host = url.host_str().unwrap_or_default().to_string();
        let path = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        let req = Request::get(&path).header("host", &host).body(Full::new(Bytes::new()))?;
        self.send(url, req)
    }

    pub fn post(&self, url: &Url, body: Bytes) -> Result<PendingResponse, HttpClientError> {
        let host = url.host_str().unwrap_or_default().to_string();
        let path = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        let len = body.len();
        let req = Request::post(&path)
            .header("host", &host)
            .header("content-type", "application/json")
            .header("content-length", len.to_string())
            .body(Full::new(body))?;
        self.send(url, req)
    }

    /// Create a reconnecting SSE stream. Call `poll()` once per loop iteration.
    pub fn sse_stream(&self, url: Url) -> SseStream {
        SseStream { tls_config: self.tls_config.clone(), url, inner: None }
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
                Ok((transport, mio)) => {
                    let host = self.url.host_str().unwrap_or_default();
                    let path = match self.url.query() {
                        Some(q) => format!("{}?{}", self.url.path(), q),
                        None => self.url.path().to_string(),
                    };
                    let req = match Request::get(&path)
                        .header("host", host)
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

#[test]
fn client_test() -> Result<(), Box<dyn std::error::Error>> {
    use std::{pin::Pin, task::Context};

    use crate::http::pending_response::noop_waker;
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
