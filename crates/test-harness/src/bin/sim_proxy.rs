//! Proxy between relay and simulator that fails `*_validateBuilderSubmission*` JSON-RPC calls
//! with a demotable error. All other requests (eth_syncing, eth_getBalance, merges) are
//! forwarded to the upstream simulator, so the relay still sees it as healthy and synced.
//!
//! With `--builder`, only that builder's submissions fail; the rest are forwarded and
//! validated normally.
//!
//! Usage: sim_proxy <listen_addr> <upstream_url> [--builder <pubkey>] [--reason <msg>]
//! e.g.   sim_proxy 127.0.0.1:8555 http://127.0.0.1:8545 --builder 0xabc..def

use std::{env, net::SocketAddr};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tracing::{error, info};

const DEFAULT_FAIL_REASON: &str = "invalid state root (test demotion)";

#[derive(Clone)]
struct Proxy {
    client: reqwest::Client,
    upstream: String,
    fail_reason: String,
    /// Only fail submissions from this builder (normalized: lowercase, no 0x prefix)
    fail_builder: Option<String>,
}

fn normalize_pubkey(pubkey: &str) -> String {
    pubkey.trim_start_matches("0x").to_ascii_lowercase()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let mut args = env::args().skip(1);
    let usage =
        "usage: sim_proxy <listen_addr> <upstream_url> [--builder <pubkey>] [--reason <msg>]";
    let listen: SocketAddr = args.next().expect(usage).parse().expect("invalid listen addr");
    let upstream = args.next().expect(usage).trim_end_matches('/').to_owned();

    let mut fail_reason = DEFAULT_FAIL_REASON.to_owned();
    let mut fail_builder = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--builder" => fail_builder = Some(normalize_pubkey(&args.next().expect(usage))),
            "--reason" => fail_reason = args.next().expect(usage),
            _ => panic!("unknown arg {arg}, {usage}"),
        }
    }

    let proxy = Proxy { client: reqwest::Client::new(), upstream, fail_reason, fail_builder };

    info!(%listen, upstream = %proxy.upstream, "sim-proxy listening");
    let app = Router::new().fallback(handle).with_state(proxy);
    let listener = tokio::net::TcpListener::bind(listen).await.expect("bind failed");
    axum::serve(listener, app).await.expect("server error");
}

async fn handle(State(proxy): State<Proxy>, uri: Uri, body: Bytes) -> Response {
    if let Ok(req) = serde_json::from_slice::<Value>(&body) {
        let method = req["method"].as_str().unwrap_or_default();
        if method.contains("_validateBuilderSubmission") {
            let builder = req["params"][0]["message"]["builder_pubkey"].as_str().unwrap_or("");
            let targeted = proxy
                .fail_builder
                .as_ref()
                .is_none_or(|target| normalize_pubkey(builder) == *target);

            if targeted {
                info!(method, builder, "failing simulation request");
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "error": { "message": proxy.fail_reason },
                });
                return axum::Json(resp).into_response();
            }
            info!(method, builder, "builder not targeted, forwarding");
        }
    }

    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{}", proxy.upstream, path);
    match proxy.client.post(&url).header("content-type", "application/json").body(body).send().await
    {
        Ok(res) => {
            let status =
                StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = res.bytes().await.unwrap_or_default();
            (status, bytes).into_response()
        }
        Err(err) => {
            error!(%err, %url, "upstream request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}
