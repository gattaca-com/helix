use std::{net::SocketAddr, time::Duration};

use axum::{
    body::Body,
    http::{header::CONTENT_TYPE, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use eyre::bail;
use lazy_static::lazy_static;
use prometheus::{
    exponential_buckets, register_gauge_vec_with_registry, register_gauge_with_registry,
    register_histogram_vec_with_registry, register_histogram_with_registry,
    register_int_counter_vec_with_registry, register_int_counter_with_registry, Encoder, Gauge,
    GaugeVec, Histogram, HistogramTimer, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry, TextEncoder,
};
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::RelayConfig;

pub async fn start_metrics_server(config: &RelayConfig) {
    let port =
        std::env::var("METRICS_PORT").map(|s| s.parse().expect("invalid port")).unwrap_or(9500);
    tokio::spawn(MetricsProvider::new(port).run());

    let opts = Opts::new("info", "Relay info")
        .const_label("version", env!("CARGO_PKG_VERSION"))
        .const_label("commit", env!("GIT_HASH"))
        .const_label("branch", env!("GIT_BRANCH"))
        .const_label("built_at", env!("BUILT_AT"))
        .const_label("region", config.postgres.region_name.to_string());

    let info = IntGauge::with_opts(opts).unwrap();
    info.set(1);

    RELAY_METRICS_REGISTRY.register(Box::new(info)).unwrap();
}

pub struct MetricsProvider {
    port: u16,
}

impl MetricsProvider {
    pub fn new(port: u16) -> Self {
        MetricsProvider { port }
    }

    pub async fn run(self) -> eyre::Result<()> {
        info!("starting metrics server on port {}", self.port);

        let router = axum::Router::new()
            .route("/metrics", get(handle_metrics))
            .route("/status", get(|| async { StatusCode::OK }));
        let address = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = TcpListener::bind(&address).await?;

        axum::serve(listener, router).await?;

        bail!("metrics server stopped")
    }
}

async fn handle_metrics() -> Response {
    match prepare_metrics() {
        Ok(response) => response,
        Err(err) => {
            error!(?err, "failed to prepare metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn prepare_metrics() -> Result<Response, MetricsError> {
    let metrics = RELAY_METRICS_REGISTRY.gather();
    let encoder = TextEncoder::new();
    let s = encoder.encode_to_string(&metrics)?;

    Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(Body::from(s))
        .map_err(MetricsError::FailedBody)
}

#[derive(Debug, thiserror::Error)]
enum MetricsError {
    #[error("failed encoding metrics {0}")]
    FailedEncoding(#[from] prometheus::Error),

    #[error("failed encoding body {0}")]
    FailedBody(#[from] axum::http::Error),
}

lazy_static! {
    pub static ref RELAY_METRICS_REGISTRY: Registry =
        Registry::new_custom(Some("helix".to_string()), None).unwrap();

    //////////////// API ////////////////

    /// Count for status codes by endpoint
    static ref REQUEST_STATUS: IntCounterVec =
        register_int_counter_vec_with_registry!(
        "request_status_total",
        "Count of status codes",
        &["endpoint", "http_status_code"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Client side timeouts by endpoint
    static ref REQUEST_TIMEOUT: IntCounterVec =
        register_int_counter_vec_with_registry!(
        "request_timeout_total",
        "Count of timeouts",
        &["endpoint"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Duration of request in seconds
    static ref REQUEST_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "request_latency_secs",
        "Latency of requests",
        &["endpoint"],
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


     /// Time spent while reading body in seconds
     static ref REQUEST_READ_BODY_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "request_body_latency_secs",
        "Time spent reading body in seconds",
        &["endpoint", "step"],
        vec![0.00005, 0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Pending requests
    static ref REQUEST_PENDING: GaugeVec = register_gauge_vec_with_registry!(
        "request_pending",
        "Pending requests",
        &["endpoint"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Request size in bytes
    static ref REQUEST_SIZE: HistogramVec = register_histogram_vec_with_registry!(
        "request_size_bytes",
        "Size of requests in bytes",
        &["endpoint"],
        exponential_buckets(1000.0, 4.0, 12).unwrap(),
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// REGISTRATION ////////////////
    pub static ref REGISTRATIONS_TO_CHECK_COUNT: IntCounter = register_int_counter_with_registry!(
        "registrations_to_check_count",
        "Count of registrations to check",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    //////////////// SIMULATOR ////////////////
    static ref SIMULATOR_COUNTS: IntCounterVec = register_int_counter_vec_with_registry!(
        "simulator_count_total",
        "Count of sim requests",
        &["is_optimistic"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIMULATOR_STATUS: IntCounterVec = register_int_counter_vec_with_registry!(
        "simulator_status_total",
        "Count of sim statuses",
        &["is_success"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIMULATOR_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "sim_latency_secs",
        "Latency of simulations",
        &["simulator"],
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_MANAGER_COUNT: IntCounterVec = register_int_counter_vec_with_registry!(
        "sim_manager_count",
        "Sim manager counts",
        &["label"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref SIM_MANAGER_GAUGE: GaugeVec = register_gauge_vec_with_registry!(
        "sim_manager_gauge",
        "Sim manager gauges",
        &["label"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref BLOCK_MERGE_STATUS: IntCounterVec = register_int_counter_vec_with_registry!(
        "block_merge_status_total",
        "Count of block merge statuses",
        &["is_success"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref BLOCK_MERGE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "block_merge_latency_secs",
        "Latency of block merging",
        &["simulator"],
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref BUILDER_DEMOTION_COUNT: IntCounter = register_int_counter_with_registry!(
        "builder_demotion_count_total",
        "Count of builder demotions",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    static ref SIMULATOR_SYNC: GaugeVec = register_gauge_vec_with_registry!(
        "simulator_synced",
        "Sync status of simulators",
        &["simulator"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    ////////////////// BEACON //////////////////

    static ref BEACON_SYNC: GaugeVec = register_gauge_vec_with_registry!(
        "beacon_synced",
        "Sync status of beacon nodes",
        &["beacon"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// GOSSIP ////////////////

    /// Received gossip messages coutn
     static ref IN_GOSSIP_COUNTS: IntCounterVec = register_int_counter_vec_with_registry!(
        "in_gossip_count_total",
        "Count of received gossip messages",
        &["endpoint"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    /// Received gossip size in bytes
    static ref IN_GOSSIP_SIZE: IntCounterVec = register_int_counter_vec_with_registry!(
        "in_gossip_size_bytes",
        "Size of receivedgossip messages",
        &["endpoint"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Sent gossip messages count
    static ref OUT_GOSSIP_COUNTS: IntCounterVec = register_int_counter_vec_with_registry!(
        "out_gossip_count_total",
        "Count of sent gossip messages",
        &["endpoint", "is_success"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Sent gossip latency
    static ref OUT_GOSSIP_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "out_gossip_latency_secs",
        "Latency of sent gossip messages",
        &["endpoint"],
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    /// Sent gossip size in bytes
    static ref OUT_GOSSIP_SIZE: HistogramVec = register_histogram_vec_with_registry!(
        "out_gossip_size",
        "Size of sent gossip messages in bytes",
        &["endpoint"],
        exponential_buckets(1000.0, 4.0, 12).unwrap(),
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// DB ////////////////
    static ref DB_COUNTS: IntCounterVec = register_int_counter_vec_with_registry!(
        "db_count_total",
        "Count of db operations",
        &["endpoint", "is_success"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref DB_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "db_latency_secs",
        "Latency of db operations",
        &["endpoint"],
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 50.0],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref TASK_COUNT: GaugeVec = register_gauge_vec_with_registry!(
        "tokio_tasks",
        "Count of spawned tasks",
        &["origin",],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref TOP_BID_CONNECTIONS: Gauge = register_gauge_with_registry!(
        "top_bid_connections",
        "Count of top bid connections",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref TOP_BID_UPDATE_COUNT: IntCounter = register_int_counter_with_registry!(
        "top_bid_update_count",
        "Count of top bid updates",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref BID_SORTER_PROCESS_LATENCY_US: Histogram = register_histogram_with_registry!(
        "bid_sorter_process_latency_us",
        "Latency of bid sorter process in us",
        vec![1., 5., 10., 25., 50., 100., 500., 1_000., 5_000., 10_000., 50_000., 100_000., 1_000_000.],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    //////////////// TIMING GAMES ////////////////

    static ref GET_HEADER_TIMEOUT: HistogramVec = register_histogram_vec_with_registry!(
        "get_header_sleep",
        "Sleep time for get header",
        &["is_timeout"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// CACHE ////////////////

    static ref DELIEVERED_PAYLOADS_CACHE_HIT: IntCounter = register_int_counter_with_registry!(
        "delivered_payloads_cache_hit",
        "Count of delivered payloads cache hits",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    static ref BIDS_CACHE_HIT: IntCounter = register_int_counter_with_registry!(
        "bids_cache_hit",
        "Count of bids cache hits",
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref BID_SIGNING_LATENCY: Histogram = register_histogram_with_registry!(
        "bid_signing_latency_us",
        "Latency of re-singing the get header bid in us",
        vec![1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 100_000.],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    /// Submission trace metrics
    pub static ref SUB_TRACE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "submission_trace_latency_us",
        "Latency of submission trace for each step",
        &["step"],
        vec![1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 25_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000., 10_000_000., 50_000_000., 100_000_000.,],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


    /// Submission trace metrics
    pub static ref GET_PAYLOAD_TRACE_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "get_payload_trace_latency_us",
        "Latency of get payload trace for each step",
        &["step"],
        vec![1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 25_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000., 10_000_000., 50_000_000., 100_000_000.,],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    //////////////// DECODING BLOCKS ////////////////

    pub static ref SUBMISSION_BY_COMPRESSION: IntCounterVec = register_int_counter_vec_with_registry!(
        "bid_submission_compression_count",
        "Count of bids by compression",
        &["compression"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref SUBMISSION_BY_ENCODING: IntCounterVec = register_int_counter_vec_with_registry!(
        "bid_submission_encoding_count",
        "Count of bids by encoding",
        &["encoding"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref SUBMISSION_COMPRESSED_BYTES: IntCounterVec = register_int_counter_vec_with_registry!(
        "bid_submission_compressed_bytes_total",
        "Compressed bytes by compression",
        &["compression"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref SUBMISSION_DECOMPRESSED_BYTES: IntCounterVec = register_int_counter_vec_with_registry!(
        "bid_submission_decompressed_bytes_total",
        "Decompressed bytes by compression",
        &["compression"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref DECOMPRESSION_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "bid_decompression_latency_us",
        "Latency of decompressing bid submissions in us",
        &["compression"],
        vec![1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 25_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000., 10_000_000., 50_000_000., 100_000_000.,],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref BID_DECODING_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "bid_decoding_latency_us",
        "Latency of decoding block payloads in us",
        &["encoding"],
        vec![1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 25_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000., 10_000_000., 50_000_000., 100_000_000.,],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref BID_DECOMPRESS_SIZEHINT_REL_ERROR: HistogramVec = register_histogram_vec_with_registry!(
        "bid_sizehint_rel_error",
        "abs(actual-estimate)/max(1, actual)",
        &["compression"],
        vec![0.005, 0.01, 0.02, 0.05, 0.10, 0.20, 0.33, 0.50, 1.0, 2.0, 5.0],
        &RELAY_METRICS_REGISTRY
    ).unwrap();

    //////////////// AUCTIONEER ////////////////
    pub static ref HYDRATION_CACHE_HITS: IntCounterVec = register_int_counter_vec_with_registry!(
        "hydration_cache_hits",
        "Count of hydration cache hits",
        &["order_type"],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();

    pub static ref STATE_TRANSITION_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "state_transition_latency_us",
        "Latency of state transition in us",
        &["start_event_end"],
        vec![0.5, 1., 5., 10., 15., 25., 50., 100., 250., 500., 1_000., 5_000., 10_000., 25_000., 50_000., 100_000., 500_000., 1_000_000., 5_000_000., 10_000_000., 50_000_000., 100_000_000.,],
        &RELAY_METRICS_REGISTRY
    )
    .unwrap();


}

pub struct ApiMetrics {
    endpoint: String,
    // records latency on drop
    _timer: HistogramTimer,
    has_completed: bool,
}

impl ApiMetrics {
    pub fn new(endpoint: String) -> Self {
        REQUEST_PENDING.with_label_values(&[endpoint.as_str()]).inc();
        let _timer = REQUEST_LATENCY.with_label_values(&[endpoint.as_str()]).start_timer();
        Self { endpoint, _timer, has_completed: false }
    }

    pub fn record(
        &mut self,
        status_code: &str,
        size: usize,
        read_latency: Duration,
        wait_latency: Duration,
        gap_latency: Duration,
    ) {
        REQUEST_STATUS.with_label_values(&[self.endpoint.as_str(), status_code]).inc();
        REQUEST_SIZE.with_label_values(&[self.endpoint.as_str()]).observe(size as f64);
        REQUEST_READ_BODY_LATENCY
            .with_label_values(&[self.endpoint.as_str(), "read"])
            .observe(read_latency.as_secs_f64());
        REQUEST_READ_BODY_LATENCY
            .with_label_values(&[self.endpoint.as_str(), "wait"])
            .observe(wait_latency.as_secs_f64());
        REQUEST_READ_BODY_LATENCY
            .with_label_values(&[self.endpoint.as_str(), "gap"])
            .observe(gap_latency.as_secs_f64());

        self.has_completed = true;
    }
}

impl Drop for ApiMetrics {
    fn drop(&mut self) {
        // decrease pending even when client cancels
        REQUEST_PENDING.with_label_values(&[self.endpoint.as_str()]).dec();
        if !self.has_completed {
            REQUEST_TIMEOUT.with_label_values(&[self.endpoint.as_str()]).inc();
        }
    }
}

pub struct GossipMetrics;

impl GossipMetrics {
    pub fn in_count(endpoint: &str) {
        IN_GOSSIP_COUNTS.with_label_values(&[endpoint]).inc();
    }

    pub fn in_size(endpoint: &str, size: usize) {
        IN_GOSSIP_SIZE.with_label_values(&[endpoint]).inc_by(size as u64);
    }

    pub fn out_count(endpoint: &str, is_success: bool) {
        OUT_GOSSIP_COUNTS.with_label_values(&[endpoint, is_success.to_string().as_str()]).inc();
    }

    /// Records on drop
    pub fn out_timer(endpoint: &str) -> HistogramTimer {
        OUT_GOSSIP_LATENCY.with_label_values(&[endpoint]).start_timer()
    }

    pub fn out_size(endpoint: &str, size: usize) {
        OUT_GOSSIP_SIZE.with_label_values(&[endpoint]).observe(size as f64);
    }
}

pub struct DbMetrics;

impl DbMetrics {
    pub fn count(endpoint: &str, is_success: bool) {
        DB_COUNTS.with_label_values(&[endpoint, is_success.to_string().as_str()]).inc();
    }

    pub fn latency(endpoint: &str) -> HistogramTimer {
        DB_LATENCY.with_label_values(&[endpoint]).start_timer()
    }
}

pub struct DbMetricRecord<'a> {
    endpoint: &'a str,
    has_recorded: bool,
    _timer: HistogramTimer,
}

impl<'a> DbMetricRecord<'a> {
    pub fn new(endpoint: &'a str) -> Self {
        let timer = DbMetrics::latency(endpoint);
        DbMetricRecord { has_recorded: false, _timer: timer, endpoint }
    }

    pub fn record_success(&mut self) {
        self.has_recorded = true;
        DbMetrics::count(self.endpoint, true);
    }

    pub fn record_failure(&mut self) {
        self.has_recorded = true;
        DbMetrics::count(self.endpoint, false);
    }
}

impl Drop for DbMetricRecord<'_> {
    fn drop(&mut self) {
        if !self.has_recorded {
            self.record_failure();
        }
    }
}

pub struct TopBidMetrics;

impl TopBidMetrics {
    pub fn connection() -> Self {
        TOP_BID_CONNECTIONS.inc();
        Self {}
    }

    pub fn top_bid_update_count() {
        TOP_BID_UPDATE_COUNT.inc();
    }
}

impl Drop for TopBidMetrics {
    fn drop(&mut self) {
        TOP_BID_CONNECTIONS.dec();
    }
}

pub struct SimulatorMetrics;

impl SimulatorMetrics {
    pub fn sim_count(is_optimistic: bool) {
        SIMULATOR_COUNTS.with_label_values(&[is_optimistic.to_string().as_str()]).inc();
    }

    pub fn sim_status(is_success: bool) {
        SIMULATOR_STATUS.with_label_values(&[is_success.to_string().as_str()]).inc();
    }

    pub fn timer(simulator: &str) -> HistogramTimer {
        SIMULATOR_LATENCY.with_label_values(&[simulator]).start_timer()
    }

    pub fn block_merge_status(is_success: bool) {
        BLOCK_MERGE_STATUS.with_label_values(&[is_success.to_string().as_str()]).inc();
    }

    pub fn block_merge_timer(simulator: &str) -> HistogramTimer {
        BLOCK_MERGE_LATENCY.with_label_values(&[simulator]).start_timer()
    }

    pub fn demotion_count() {
        BUILDER_DEMOTION_COUNT.inc();
    }

    pub fn simulator_sync(simulator: &str, is_synced: bool) {
        SIMULATOR_SYNC.with_label_values(&[simulator]).set(is_synced as i64 as f64);
    }

    pub fn sim_mananger_count(label: &str, count: usize) {
        SIM_MANAGER_COUNT.with_label_values(&[label]).inc_by(count as u64);
    }

    pub fn sim_manager_gauge(label: &str, val: usize) {
        SIM_MANAGER_GAUGE.with_label_values(&[label]).set(val as f64);
    }
}

pub struct BeaconMetrics;

impl BeaconMetrics {
    pub fn beacon_sync(beacon: &str, is_synced: bool) {
        BEACON_SYNC.with_label_values(&[beacon]).set(is_synced as i64 as f64);
    }
}

pub struct GetHeaderMetric {
    sleep_time: f64,
    has_recorded: bool,
}

impl GetHeaderMetric {
    pub fn new(sleep_time: Duration) -> Self {
        Self { sleep_time: sleep_time.as_secs_f64(), has_recorded: false }
    }

    pub fn record(&mut self) {
        self.has_recorded = true;
    }
}

impl Drop for GetHeaderMetric {
    fn drop(&mut self) {
        let is_timeout = !self.has_recorded;

        GET_HEADER_TIMEOUT
            .with_label_values(&[is_timeout.to_string().as_str()])
            .observe(self.sleep_time);
    }
}

pub fn delivered_payloads_cache_hit() {
    DELIEVERED_PAYLOADS_CACHE_HIT.inc();
}

pub fn bids_cache_hit() {
    BIDS_CACHE_HIT.inc();
}
