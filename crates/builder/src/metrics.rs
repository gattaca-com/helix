//! Prometheus metrics for the merging role. The names follow the questions the
//! design needs answered: how much value the pool actually yields, how stale
//! our base is when we emit, and whether the merged bid beats the base
//! builder's own next bid.
#![allow(clippy::disallowed_types)]

use std::net::SocketAddr;

use alloy_primitives::U256;
use axum::{Router, http::StatusCode, routing::get};
use lazy_static::lazy_static;
use prometheus::{
    Encoder, HistogramVec, IntCounterVec, Registry, TextEncoder,
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry,
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
        10., 50., 100., 250., 500., 1_000., 2_500., 5_000., 10_000., 25_000., 50_000., 100_000.,
        250_000., 500_000., 1_000_000.,
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

    //////////////// PIPELINE ////////////////

    /// Critical-path cost by stage: replay, extend, emit, and the activation
    /// queue wait.
    static ref STAGE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "merge_stage_latency_us",
        "Merge pipeline latency by stage",
        &["stage"],
        micros_buckets(),
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    /// Speculative replay bookkeeping, and where activations were served from.
    static ref SPECULATION: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_speculation_total",
        "Speculative replay events by outcome",
        &["outcome"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref ACTIVATION_SOURCE: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_activation_source_total",
        "Merge session activations by where the session came from",
        &["source"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();

    static ref REJECTION: IntCounterVec = register_int_counter_vec_with_registry!(
        "merge_rejection_total",
        "Blocks and activations refused, by reason",
        &["stage", "reason"],
        &BUILDER_METRICS_REGISTRY
    )
    .unwrap();
}

/// U256 wei as gwei, saturating rather than panicking on an absurd value.
pub fn gwei(v: U256) -> f64 {
    let wei: f64 = if v > U256::from(u128::MAX) { u128::MAX as f64 } else { v.to::<u128>() as f64 };
    wei / WEI_PER_GWEI
}

pub fn order_outcome(outcome: &str, value: U256) {
    ORDER_OUTCOME.with_label_values(&[outcome]).inc();
    ORDER_VALUE.with_label_values(&[outcome]).observe(gwei(value));
}

pub fn emit_value(total_revenue: U256, proposer_delta: U256) {
    EMIT_VALUE.with_label_values(&["total_revenue"]).observe(gwei(total_revenue));
    EMIT_VALUE.with_label_values(&["proposer_delta"]).observe(gwei(proposer_delta));
}

pub fn base_age(stage: &str, age_ms: u64) {
    BASE_AGE.with_label_values(&[stage]).observe(age_ms as f64);
}

pub fn beats_own_bid(reference: &str, beats: bool) {
    BEATS_OWN_BID.with_label_values(&[reference, if beats { "yes" } else { "no" }]).inc();
}

pub fn ratchet(delta: U256, interval_ms: u64, rising: bool) {
    let direction = if rising { "up" } else { "down" };
    RATCHET_VALUE.with_label_values(&[direction]).observe(gwei(delta));
    RATCHET_INTERVAL.with_label_values(&[direction]).observe(interval_ms as f64);
}

/// One emission's full verdict: whether it beat the builder's own stream, how
/// far behind it was, and how much headroom it actually had.
pub fn emission_verdict(
    won: bool,
    steps_behind: u64,
    base_age_ms: u64,
    budget_steps: u64,
    budget_ms: u64,
    deadline_seen: bool,
) {
    let result = if won { "won" } else { "lost" };
    let deadline = if deadline_seen { "seen" } else { "unseen" };
    STEPS_BEHIND.with_label_values(&[result]).observe(steps_behind as f64);
    BUDGET.with_label_values(&[result, deadline]).observe(budget_ms as f64);
    BUDGET_STEPS.with_label_values(&[result, deadline]).observe(budget_steps as f64);
    BASE_AGE.with_label_values(&[result]).observe(base_age_ms as f64);
}

pub fn stage_latency(stage: &str, micros: u64) {
    STAGE_LATENCY.with_label_values(&[stage]).observe(micros as f64);
}

/// A live session moved onto a fresher base from the same builder.
pub fn rebase() {
    SPECULATION.with_label_values(&["rebase"]).inc();
}

pub fn speculation(outcome: &str) {
    SPECULATION.with_label_values(&[outcome]).inc();
}

pub fn speculation_by(outcome: &str, n: u64) {
    SPECULATION.with_label_values(&[outcome]).inc_by(n);
}

pub fn activation_source(source: &str) {
    ACTIVATION_SOURCE.with_label_values(&[source]).inc();
}

pub fn rejection(stage: &str, reason: &str) {
    REJECTION.with_label_values(&[stage, reason]).inc();
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
