//! Prometheus metrics for the merging and simulation roles. The merging names
//! follow the questions the design needs answered: how much value the pool
//! actually yields, how stale our base is when we emit, and whether the merged
//! bid beats the base builder's own next bid.
#![allow(clippy::disallowed_types)]

use std::{net::SocketAddr, time::Instant};

use alloy_primitives::U256;
use axum::{Router, http::StatusCode, routing::get};
use lazy_static::lazy_static;
use prometheus::{
    Encoder, Histogram, HistogramVec, IntCounterVec, IntGauge, Registry, TextEncoder,
    register_histogram_vec_with_registry, register_histogram_with_registry,
    register_int_counter_vec_with_registry, register_int_gauge_with_registry,
};
use tokio::net::TcpListener;
use tracing::{error, info};

const WEI_PER_GWEI: f64 = 1e9;

/// Value buckets in gwei, spanning dust to ~10 ETH.
fn value_buckets() -> Vec<f64> {
    vec![0.0, 1e3, 1e4, 1e5, 1e6, 5e6, 1e7, 5e7, 1e8, 5e8, 1e9, 5e9, 1e10, 5e10, 1e11]
}

fn micros_buckets() -> Vec<f64> {
    vec![
        10., 25., 50., 100., 200., 300., 500., 750., 1_000., 1_500., 2_000., 3_000., 5_000.,
        7_500., 10_000., 15_000., 20_000., 30_000., 50_000., 100_000., 250_000., 500_000.,
        1_000_000.,
    ]
}

fn millis_buckets() -> Vec<f64> {
    vec![1., 5., 10., 25., 50., 75., 100., 150., 200., 250., 400., 600., 1_000., 2_000., 6_000.]
}

lazy_static! {
    pub static ref BUILDER_METRICS_REGISTRY: Registry =
        Registry::new_custom(Some("helix_builder".to_string()), None).unwrap();

    //////////////// VALUE EXTRACTION ////////////////

    /// Candidate orders by screening outcome. `applied` against the rest is the
    /// extraction rate; `revert_not_allowed` is expected to dominate.
    static ref ORDER_OUTCOME: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_order_outcome_total",
        "Candidate merge orders by screening outcome",
        &["outcome"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Priority-fee headroom of each candidate, by outcome. The area under
    /// `revert_not_allowed` is the prize an ordering-aware merge could target.
    static ref ORDER_VALUE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_order_value_gwei",
        "Upper-bound priority fee of a candidate order, by screening outcome",
        &["outcome"],
        value_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Merged revenue per emission, split into the total extracted and the
    /// proposer's share. The proposer share is what must beat the ratchet.
    static ref EMIT_VALUE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_emit_value_gwei",
        "Value of an emitted merged block, by component",
        &["component"],
        value_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// STALENESS AND THE RACE ////////////////

    /// Age of the base block when we emit. The relay drops anything past
    /// `max_merged_bid_age_ms`, so this distribution must sit under it.
    static ref BASE_AGE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_base_age_ms",
        "Age of the base block at a given stage",
        &["stage"],
        millis_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Whether the emitted merged bid beats its own base builder's bid. The win
    /// predictor: the comparison the relay makes at get_header, run at emission
    /// time. `reference="latest"` is what the relay actually compares against,
    /// since the bid sorter honours cancellations; `best` is the upper bound.
    static ref BEATS_OWN_BID: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_beats_own_bid_total",
        "Emissions by whether they beat the base builder's own bid",
        &["reference", "result"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Bid growth between consecutive submissions from one builder: R, the bar
    /// the proposer share must clear.
    static ref RATCHET_VALUE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_ratchet_gwei",
        "Bid value change between consecutive submissions from one builder",
        &["direction"],
        value_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Time between consecutive submissions from one builder, pairing with
    /// `merge_ratchet_gwei` to give the ratchet rate.
    static ref RATCHET_INTERVAL: HistogramVec = register_histogram_vec_with_registry!(
        "merge_ratchet_interval_ms",
        "Time between consecutive submissions from one builder",
        &["direction"],
        millis_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// How far behind the builder's own stream the emitted base was, in
    /// submissions. `s` in the win condition.
    static ref STEPS_BEHIND: HistogramVec = register_histogram_vec_with_registry!(
        "merge_steps_behind",
        "Submissions the emitted base lagged its builder's stream by",
        &["result"],
        vec![0., 1., 2., 3., 5., 8., 13., 21., 34., 55., 89., 144., 233., 400.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// How long we actually had to emit and still beat this builder's stream,
    /// computed exactly from its bid history rather than from mean drift.
    /// `deadline="seen"` is an exact figure; `unseen` is a lower bound, the
    /// builder had not yet out-bid us when the sample was taken.
    static ref BUDGET: HistogramVec = register_histogram_vec_with_registry!(
        "merge_budget_ms",
        "Time available to emit and still beat the base builder's own stream",
        &["result", "deadline"],
        millis_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref BUDGET_STEPS: HistogramVec = register_histogram_vec_with_registry!(
        "merge_budget_steps",
        "Submissions of headroom before the base builder out-bids our merge",
        &["result", "deadline"],
        vec![0., 1., 2., 3., 5., 8., 13., 21., 34., 55., 89., 144., 233., 400.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Every millisecond between a base arriving and the merge being emitted,
    /// split by phase. The phases sum to the base age, so waiting shows up as
    /// clearly as work does.
    static ref PHASE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_phase_ms",
        "Time a base spends in each phase between arrival and emission",
        &["phase"],
        millis_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// How many times a base was improved before a fresher one replaced it.
    /// One means we abandoned it immediately; the value left in the pool is
    /// what makes that costly.
    static ref EXTEND_PASSES: Histogram = register_histogram_with_registry!(
        "merge_extend_passes",
        "Extend and emit passes made on one base",
        vec![1., 2., 3., 4., 5., 7., 10., 15., 20., 30., 50.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Why a stream stopped improving a base and took another.
    static ref REBASE_REASON: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_rebase_reason_total",
        "Why a stream gave up the base it held",
        &["reason"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Delta reached on the first pass of a new base, against the delta the
    /// previous base ended on. Two distributions, so the comparison is not a
    /// mean of unbounded per-event ratios.
    static ref DELTA_ON_BASE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_delta_on_base_gwei",
        "Delta on a base at first pass and at hand-off",
        &["stage"],
        value_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Emissions that beat the builder's own bid *and* are inside the relay's
    /// staleness gate. The joint condition is the objective.
    static ref SERVABLE_WIN: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_servable_win_total",
        "Emissions that beat the builder's bid and are young enough to serve",
        &["servable"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref EMIT_BASE: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_emit_base_total",
        "Emissions by whether a newer block from the base's builder was already waiting",
        &["base"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Size of the state-root walk, by stage and unit.
    static ref STATE_WALK: HistogramVec = register_histogram_vec_with_registry!(
        "merge_state_walk",
        "Accounts and storage slots the state-root walk touches",
        &["stage", "unit"],
        vec![1., 5., 10., 25., 50., 100., 200., 400., 800., 1_600., 3_200., 6_400., 12_800.],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// PIPELINE ////////////////

    static ref STAGE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "merge_stage_latency_us",
        "Merge pipeline latency by stage",
        &["stage"],
        micros_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref SPECULATION: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_speculation_total",
        "Speculative replay events by outcome",
        &["outcome"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref LAYER_BUILD: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_layer_build_total",
        "State layer builds by whether they started from the builder's previous base",
        &["outcome"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref WAIT_PHASE: HistogramVec = register_histogram_vec_with_registry!(
        "merge_wait_phase_ms",
        "Time a waiting base spent behind each phase of its stream's previous base",
        &["phase"],
        millis_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref WAIT_DOMINANT: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_wait_dominant_total",
        "Waits of 5ms or more, by the phase that took most of them",
        &["phase"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref TX_REUSE: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_tx_reuse_total",
        "Txs by stage and whether an earlier result was reused",
        &["stage", "outcome"],
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

    static ref EXTEND_ORDERS: HistogramVec = register_histogram_vec_with_registry!(
        "merge_extend_orders",
        "Orders per extend pass, by step",
        &["step"],
        vec![0., 1., 2., 4., 8., 16., 32., 64., 128., 256., 512., 1_024., 2_048., 4_096.],
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

/// U256 wei as gwei, saturating rather than panicking on an absurd value.
pub fn gwei(v: U256) -> f64 {
    let wei: f64 = if v > U256::from(u128::MAX) { u128::MAX as f64 } else { v.to::<u128>() as f64 };
    wei / WEI_PER_GWEI
}

pub fn ratchet(delta: U256, interval_ms: u64, rising: bool) {
    let direction = if rising { "up" } else { "down" };
    RATCHET_VALUE.with_label_values(&[direction]).observe(gwei(delta));
    RATCHET_INTERVAL.with_label_values(&[direction]).observe(interval_ms as f64);
}

thread_local! {
    static WARMING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Latency metrics recorded on this thread are dropped while a warm-up base is processed.
pub fn set_warming(warming: bool) {
    WARMING.with(|w| w.set(warming));
}

pub fn warming() -> bool {
    WARMING.with(|w| w.get())
}

pub fn stage_latency(stage: &str, micros: u64) {
    if warming() {
        return;
    }
    STAGE_LATENCY.with_label_values(&[stage]).observe(micros as f64);
}

pub fn speculation(outcome: &str) {
    SPECULATION.with_label_values(&[outcome]).inc();
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
