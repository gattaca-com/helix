//! Prometheus metrics for the merging and simulation roles.
#![allow(clippy::disallowed_types)]

use std::{net::SocketAddr, time::Instant};

use axum::{Router, http::StatusCode, routing::get};
use lazy_static::lazy_static;
use prometheus::{
    Encoder, HistogramVec, IntCounterVec, IntGauge, Registry, TextEncoder,
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry,
    register_int_gauge_with_registry,
};
use tokio::net::TcpListener;
use tracing::{error, info};

fn micros_buckets() -> Vec<f64> {
    vec![
        10., 25., 50., 100., 200., 300., 500., 750., 1_000., 1_500., 2_000., 3_000., 5_000.,
        7_500., 10_000., 15_000., 20_000., 30_000., 50_000., 100_000., 250_000., 500_000.,
        1_000_000.,
    ]
}

lazy_static! {
    pub static ref BUILDER_METRICS_REGISTRY: Registry =
        Registry::new_custom(Some("helix_builder".to_string()), None).unwrap();

    //////////////// PIPELINE ////////////////

    static ref STAGE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "merge_stage_latency_us",
        "Merge pipeline latency by stage",
        &["stage"],
        micros_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref REJECTION: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_rejection_total",
        "Blocks refused, by reason",
        &["stage", "reason"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref INGEST: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_ingest_total",
        "Frames accepted from the relay, by kind",
        &["kind"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref REJECT_SENT: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_reject_sent_total",
        "RejectV1 and FatalV1 frames sent to the relay, by code",
        &["code", "severity"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref QUEUE_DROP: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_queue_drop_total",
        "Work dropped because an internal queue was full",
        &["queue"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SLOT_POOL: HistogramVec = register_histogram_vec_with_registry!(
        "merge_slot_pool",
        "Size of the merge pool at slot teardown, by kind",
        &["kind"],
        vec![0., 1., 2., 4., 8., 16., 32., 64., 128., 256., 512., 1_024., 2_048., 4_096.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref RELAY_CONNECTED: IntGauge = register_int_gauge_with_registry!(
        "merge_relay_connected",
        "Whether a relay has completed the merging handshake",
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// SIMULATION ////////////////

    static ref SIM_STAGE: HistogramVec = register_histogram_vec_with_registry!(
        "sim_stage_latency_us",
        "Block validation latency by stage",
        &["stage"],
        micros_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_REQUEST: HistogramVec = register_histogram_vec_with_registry!(
        "sim_request_latency_us",
        "Validation request latency from body received to response, by route and result",
        &["route", "result"],
        micros_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_BLOCK: HistogramVec = register_histogram_vec_with_registry!(
        "sim_block_size",
        "Size of each executed block, by unit",
        &["unit"],
        vec![0., 10., 25., 50., 100., 200., 300., 500., 750., 1_000., 1_500., 2_000., 3_000.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_BLOB_CACHE: IntCounterVec = register_int_counter_vec_with_registry!(
        "sim_blob_cache_total",
        "Blobs checked against the verified-blob cache, by result",
        &["result"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_IN_FLIGHT: IntGauge = register_int_gauge_with_registry!(
        "sim_in_flight",
        "Validations running on the blocking pool",
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();
}

pub fn ingest(kind: &str) {
    INGEST.with_label_values(&[kind]).inc();
}

pub fn reject_sent(code: &str, fatal: bool) {
    REJECT_SENT.with_label_values(&[code, if fatal { "fatal" } else { "reject" }]).inc();
}

pub fn queue_drop(queue: &str) {
    QUEUE_DROP.with_label_values(&[queue]).inc();
}

pub fn slot_pool(blocks: usize, orders: usize, builders: usize) {
    SLOT_POOL.with_label_values(&["blocks"]).observe(blocks as f64);
    SLOT_POOL.with_label_values(&["orders"]).observe(orders as f64);
    SLOT_POOL.with_label_values(&["builders"]).observe(builders as f64);
}

pub fn relay_connected(connected: bool) {
    RELAY_CONNECTED.set(connected as i64);
}

pub fn stage_latency(stage: &str, micros: u64) {
    STAGE_LATENCY.with_label_values(&[stage]).observe(micros as f64);
}

pub fn rejection(stage: &str, reason: &str) {
    REJECTION.with_label_values(&[stage, reason]).inc();
}

pub fn sim_lap(stage: &str, since: Instant) -> Instant {
    let now = Instant::now();
    SIM_STAGE.with_label_values(&[stage]).observe(now.duration_since(since).as_micros() as f64);
    now
}

pub fn sim_request(route: &str, result: &str, since: Instant) {
    SIM_REQUEST.with_label_values(&[route, result]).observe(since.elapsed().as_micros() as f64);
}

pub fn sim_block(txs: usize, gas_used: u64) {
    SIM_BLOCK.with_label_values(&["txs"]).observe(txs as f64);
    SIM_BLOCK.with_label_values(&["mgas"]).observe(gas_used as f64 / 1e6);
}

pub fn sim_blob_cache(hits: usize, misses: usize) {
    SIM_BLOB_CACHE.with_label_values(&["hit"]).inc_by(hits as u64);
    SIM_BLOB_CACHE.with_label_values(&["miss"]).inc_by(misses as u64);
}

/// Held by the blocking task, so a request its client abandoned still counts.
pub struct SimInFlight;

impl SimInFlight {
    pub fn enter() -> Self {
        SIM_IN_FLIGHT.inc();
        Self
    }
}

impl Drop for SimInFlight {
    fn drop(&mut self) {
        SIM_IN_FLIGHT.dec();
    }
}

pub async fn serve(port: u16) {
    let router = Router::new()
        .route("/metrics", get(handle_metrics))
        .route("/status", get(|| async { StatusCode::OK }));
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    match TcpListener::bind(&address).await {
        Ok(listener) => {
            info!(port, "builder metrics server listening");
            if let Err(err) = axum::serve(listener, router).await {
                error!(%err, "builder metrics server exited");
            }
        }
        Err(err) => error!(%err, port, "failed to bind builder metrics server"),
    }
}

async fn handle_metrics() -> (StatusCode, String) {
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();
    match encoder.encode(&BUILDER_METRICS_REGISTRY.gather(), &mut buffer) {
        Ok(()) => match String::from_utf8(buffer) {
            Ok(body) => (StatusCode::OK, body),
            Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        },
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}
