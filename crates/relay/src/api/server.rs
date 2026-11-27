//! The HTTP listener an API tile serves its routes from.

use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    time::{Duration, Instant},
};

use flux_network::{
    Network, Token,
    http::{HttpEvent, HttpNetwork, HttpRequest},
};
use governor::{
    DefaultKeyedRateLimiter, Quota, RateLimiter,
    clock::{Clock, DefaultClock},
};
use helix_common::{Route, RouterConfig, metrics::ApiMetrics, utils::utcnow_ns};
use http::{HeaderMap, HeaderName, HeaderValue};
use rustc_hash::FxHashMap;
use tracing::info;
use uuid::Uuid;

const X_REQUEST_ID: &str = "x-request-id";
const TEXT: (&str, &str) = ("content-type", "text/plain; charset=utf-8");
/// Headers a handler may add to a response, besides the request id.
const MAX_RESPONSE_HEADERS: usize = 3;
const LIMITER_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

struct ServedRoute {
    route: Route,
    method: &'static str,
    /// The route's path as configured, for metrics.
    path: String,
    /// Length of the path up to its first parameter, and how many parameters follow.
    prefix: usize,
    params: usize,
    limiter: Option<DefaultKeyedRateLimiter<IpAddr>>,
}

impl ServedRoute {
    /// The parameter segments of `path` when it is this route's.
    fn params_of<'a>(&self, path: &'a str) -> Option<&'a str> {
        let rest = path.strip_prefix(&self.path[..self.prefix])?;
        let matches = if self.params == 0 {
            rest.is_empty()
        } else {
            rest.split('/').count() == self.params && rest.split('/').all(|s| !s.is_empty())
        };
        matches.then_some(rest)
    }
}

/// What it takes to answer a request. Dropped unanswered, it counts as a timeout.
pub(crate) struct Responder {
    pub token: Token,
    /// The caller's `x-request-id`, echoed as sent; ours when it sent none.
    pub request_id: HeaderValue,
    /// `None` for a request that matched no route.
    metrics: Option<ApiMetrics>,
}

pub(crate) struct Inbound {
    pub route: Route,
    pub peer: SocketAddr,
    pub received_ns: u64,
    pub headers: HeaderMap,
    /// The path's parameter segments, `/`-separated.
    pub params: String,
    pub responder: Responder,
}

pub(crate) struct ApiServer {
    net: Network,
    http: HttpNetwork,
    routes: Vec<ServedRoute>,
    peers: FxHashMap<Token, SocketAddr>,
    /// Requests refused while the listener was borrowed for delivery.
    refused: Vec<(Responder, u16, u64)>,
    next_prune: Instant,
}

impl ApiServer {
    /// Serves the routes of `served` that `config` enables. HTTP/1.1 only: an HTTP/2
    /// stream reset is not reported, and a handler holding a request needs to learn
    /// that its caller gave up.
    pub fn new(
        name: &'static str,
        addrs: &[SocketAddr],
        config: &RouterConfig,
        served: &[(Route, &'static str)],
    ) -> Self {
        let mut config = config.clone();
        config.resolve_condensed_routes();

        let mut routes = Vec::new();
        for info in &config.enabled_routes {
            let Some((route, method)) = served.iter().find(|(route, _)| *route == info.route)
            else {
                continue;
            };
            let path = route.path();
            let prefix = path.find('{').unwrap_or(path.len());
            let limiter = info.rate_limit.as_ref().map(|limit| {
                let quota = Quota::with_period(Duration::from_millis(limit.replenish_ms))
                    .expect("rate limit replenish_ms must be positive")
                    .allow_burst(
                        NonZeroU32::new(limit.burst_size)
                            .expect("rate limit burst_size must be positive"),
                    );
                RateLimiter::keyed(quota)
            });
            let params = path[prefix..].split('/').filter(|s| !s.is_empty()).count();
            routes.push(ServedRoute { route: *route, method, path, prefix, params, limiter });
        }

        let mut net = Network::default();
        // A load balancer keeps idle connections open for reuse; closing one under it
        // loses the request it sends next.
        let mut http = HttpNetwork::default().with_name(name).without_idle_timeout();
        for addr in addrs {
            http.listen(&mut net, *addr)
                .unwrap_or_else(|e| panic!("failed to bind the {name} API on {addr}: {e}"));
            info!(%addr, name, "API tile listening");
        }

        Self {
            net,
            http,
            routes,
            peers: FxHashMap::default(),
            refused: Vec::new(),
            next_prune: Instant::now() + LIMITER_PRUNE_INTERVAL,
        }
    }

    /// Appends the requests that arrived to `requests` and answers those no route
    /// takes. `closed` gets the connections that went away, which may include the
    /// caller of one of this call's requests: a request can arrive together with its
    /// connection's close. Tokens are never reused, so the two lists need no order.
    /// Returns whether anything happened.
    pub fn poll(&mut self, requests: &mut Vec<Inbound>, closed: &mut Vec<Token>) -> bool {
        let http = &mut self.http;
        let mut worked = self.net.poll_with(|event| {
            http.on_event(&event);
        });

        let routes = &self.routes;
        let peers = &mut self.peers;
        let refused = &mut self.refused;
        self.http.drive(&mut self.net, |event| match event {
            HttpEvent::Accepted { token, peer_addr } => {
                peers.insert(token, peer_addr);
            }
            HttpEvent::Disconnected { token } => {
                peers.remove(&token);
                closed.push(token);
            }
            HttpEvent::Request { token, request } => {
                let received_ns = utcnow_ns();
                let Some(peer) = peers.get(&token).copied() else { return };
                let mut headers = header_map(&request);
                let request_id = headers
                    .get(X_REQUEST_ID)
                    .filter(|id| id.to_str().is_ok())
                    .cloned()
                    .unwrap_or_else(|| {
                        let mut id = Uuid::encode_buffer();
                        let id = Uuid::new_v4().as_hyphenated().encode_lower(&mut id);
                        HeaderValue::from_str(id).expect("a uuid is a valid header value")
                    });
                // Handlers see the id among the headers, as behind axum's request-id layer.
                headers.insert(X_REQUEST_ID, request_id.clone());
                let path = request.path.split('?').next().unwrap_or_default();

                let mut status = 404;
                for served in routes {
                    let Some(params) = served.params_of(path) else { continue };
                    // A route that answers GET answers HEAD; the listener drops the body.
                    let head = served.method == "GET" && request.method == "HEAD";
                    if served.method != request.method && !head {
                        status = 405;
                        continue;
                    }
                    let responder = Responder {
                        token,
                        request_id,
                        metrics: Some(ApiMetrics::new(served.path.clone())),
                    };
                    let limited = served
                        .limiter
                        .as_ref()
                        .and_then(|limiter| limiter.check_key(&client_ip(&headers, peer)).err());
                    if let Some(until) = limited {
                        let wait = until.wait_time_from(DefaultClock::default().now());
                        refused.push((responder, 429, wait.as_secs()));
                        return;
                    }
                    requests.push(Inbound {
                        route: served.route,
                        peer,
                        received_ns,
                        headers,
                        params: params.to_owned(),
                        responder,
                    });
                    return;
                }
                refused.push((Responder { token, request_id, metrics: None }, status, 0));
            }
            _ => {}
        });

        worked |= !self.refused.is_empty();
        while let Some((responder, status, wait_secs)) = self.refused.pop() {
            if status == 429 {
                let wait = wait_secs.to_string();
                let body = format!("Too Many Requests! Wait for {wait}s");
                let headers = [("x-ratelimit-after", wait.as_str()), ("retry-after", &wait)];
                self.respond(responder, status, &headers, body.as_bytes());
            } else {
                self.respond(responder, status, &[], &[]);
            }
        }

        let now = Instant::now();
        if now >= self.next_prune {
            self.next_prune = now + LIMITER_PRUNE_INTERVAL;
            for limiter in self.routes.iter().filter_map(|served| served.limiter.as_ref()) {
                limiter.retain_recent();
            }
        }
        worked
    }

    /// Answers a request with up to [`MAX_RESPONSE_HEADERS`] headers of the caller's.
    pub fn respond(
        &mut self,
        responder: Responder,
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
    ) {
        let request_id = responder.request_id.to_str().unwrap_or_default();
        let mut all = [(X_REQUEST_ID, request_id); MAX_RESPONSE_HEADERS + 1];
        all[1..=headers.len()].copy_from_slice(headers);
        self.http.respond(&mut self.net, responder.token, status, &all[..=headers.len()], body);

        if let Some(mut metrics) = responder.metrics {
            let no_body = Duration::ZERO;
            metrics.record(&status.to_string(), 0, no_body, no_body, no_body);
        }
    }

    pub fn respond_text(&mut self, responder: Responder, status: u16, body: &str) {
        self.respond(responder, status, &[TEXT], body.as_bytes());
    }
}

fn header_map(request: &HttpRequest<'_>) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(request.headers.len());
    for header in request.headers {
        if let (Ok(name), Ok(value)) =
            (HeaderName::from_bytes(header.name.as_bytes()), HeaderValue::from_bytes(header.value))
        {
            headers.append(name, value);
        }
    }
    headers
}

/// The address a rate limit is keyed on: the caller a proxy names, else the socket peer.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    let named = |name: &str| headers.get(name)?.to_str().ok();
    named("x-forwarded-for")
        .and_then(|list| list.split(',').find_map(|ip| ip.trim().parse().ok()))
        .or_else(|| named("x-real-ip")?.trim().parse().ok())
        .or_else(|| {
            let lines = headers.get_all("forwarded").iter().filter_map(|line| line.to_str().ok());
            lines.flat_map(|line| line.split([',', ';'])).find_map(forwarded_for)
        })
        .unwrap_or(peer.ip())
}

/// The address in one `for=` part of a `Forwarded` header, with or without a port.
fn forwarded_for(part: &str) -> Option<IpAddr> {
    let (name, node) = part.trim().split_once('=')?;
    if !name.eq_ignore_ascii_case("for") {
        return None;
    }
    let node = node.trim_matches('"');
    node.parse::<SocketAddr>()
        .map(|addr| addr.ip())
        .or_else(|_| node.trim_matches(['[', ']']).parse())
        .ok()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{ErrorKind, Read, Write},
        net::{TcpListener, TcpStream},
    };

    use helix_common::{RateLimitInfo, RouteInfo};

    use super::*;

    fn served(route: Route) -> ServedRoute {
        let path = route.path();
        let prefix = path.find('{').unwrap_or(path.len());
        let params = path[prefix..].split('/').filter(|s| !s.is_empty()).count();
        ServedRoute { route, method: "GET", path, prefix, params, limiter: None }
    }

    /// Sends `raw` on a fresh connection and returns the whole response, answering
    /// routed requests with 200.
    fn exchange(server: &mut ApiServer, addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();

        let mut requests = Vec::new();
        let mut response = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no response to {raw:?}");
            server.poll(&mut requests, &mut Vec::new());
            for request in requests.drain(..) {
                // Handlers read the id from the headers, as behind axum.
                assert_eq!(request.headers.get(X_REQUEST_ID), Some(&request.responder.request_id));
                assert_eq!(request.headers.keys().last().unwrap(), X_REQUEST_ID);
                server.respond(request.responder, 200, &[], &[]);
            }
            let mut chunk = [0; 1024];
            match stream.read(&mut chunk) {
                Ok(0) => return String::from_utf8(response).unwrap(),
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn requests_are_routed_limited_and_tagged() {
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let config = RouterConfig {
            enabled_routes: vec![RouteInfo {
                route: Route::Status,
                rate_limit: Some(RateLimitInfo { replenish_ms: 60_000, burst_size: 2 }),
            }],
            shutdown_delay_ms: 0,
        };
        let served = [(Route::Status, "GET"), (Route::GetHeader, "GET")];
        let mut server = ApiServer::new("test", &[addr], &config, &served);
        let get = |path: &str, extra: &str| {
            format!("GET {path} HTTP/1.1\r\nhost: relay\r\nconnection: close\r\n{extra}\r\n")
        };

        let id = "x-request-id: Caller-Chosen-7\r\n";
        let ok = exchange(&mut server, addr, &get("/eth/v1/builder/status?x=1", id));
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains(id), "the caller's request id is echoed: {ok}");

        let head = get("/eth/v1/builder/status", "").replacen("GET", "HEAD", 1);
        assert!(exchange(&mut server, addr, &head).starts_with("HTTP/1.1 200"));
        let post = get("/eth/v1/builder/status", "").replacen("GET", "POST", 1);
        assert!(exchange(&mut server, addr, &post).starts_with("HTTP/1.1 405"));
        assert!(exchange(&mut server, addr, &get("/nope", "")).starts_with("HTTP/1.1 404"));
        // Enabled by the tile but not by the config.
        let header = get("/eth/v1/builder/header/1/0xab/0xcd", "");
        assert!(exchange(&mut server, addr, &header).starts_with("HTTP/1.1 404"));

        // The limit is per caller as the proxy names it, not per connection.
        let from = |ip: &str| get("/eth/v1/builder/status", &format!("x-forwarded-for: {ip}\r\n"));
        assert!(exchange(&mut server, addr, &from("9.9.9.9")).starts_with("HTTP/1.1 200"));
        assert!(exchange(&mut server, addr, &from("9.9.9.9")).starts_with("HTTP/1.1 200"));
        let limited = exchange(&mut server, addr, &from("9.9.9.9"));
        assert!(limited.starts_with("HTTP/1.1 429"), "{limited}");
        assert!(exchange(&mut server, addr, &from("8.8.8.8")).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn the_caller_is_the_one_a_proxy_names() {
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let ip = |name: &'static str, value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static(value));
            client_ip(&headers, peer).to_string()
        };
        assert_eq!(ip("x-forwarded-for", "unknown, 9.9.9.9, 8.8.8.8"), "9.9.9.9");
        assert_eq!(ip("x-real-ip", "9.9.9.9"), "9.9.9.9");
        assert_eq!(ip("forwarded", "by=1.1.1.1;For=9.9.9.9;proto=https, for=8.8.8.8"), "9.9.9.9");
        assert_eq!(ip("forwarded", "for=\"9.9.9.9:4711\""), "9.9.9.9");
        assert_eq!(ip("forwarded", "for=\"[2001:db8::1]:4711\""), "2001:db8::1");
        assert_eq!(ip("forwarded", "for=\"[2001:db8::1]\""), "2001:db8::1");
        assert_eq!(ip("forwarded", "for=_hidden"), "10.0.0.1");

        let mut two_lines = HeaderMap::new();
        two_lines.append("forwarded", HeaderValue::from_static("for=unknown"));
        two_lines.append("forwarded", HeaderValue::from_static("for=9.9.9.9"));
        assert_eq!(client_ip(&two_lines, peer).to_string(), "9.9.9.9");
    }

    #[test]
    fn a_path_matches_only_its_own_route() {
        let status = served(Route::Status);
        assert_eq!(status.params_of("/eth/v1/builder/status"), Some(""));
        assert_eq!(status.params_of("/eth/v1/builder/status/x"), None);

        let header = served(Route::GetHeader);
        assert_eq!(header.params_of("/eth/v1/builder/header/1/0xab/0xcd"), Some("1/0xab/0xcd"));
        assert_eq!(header.params_of("/eth/v1/builder/header/1/0xab"), None);
        assert_eq!(header.params_of("/eth/v1/builder/header/1/0xab/0xcd/extra"), None);
        assert_eq!(header.params_of("/eth/v1/builder/header/1//0xcd"), None);
        assert_eq!(header.params_of("/eth/v1/builder/header_stream/1/0xab/0xcd"), None);
    }
}
