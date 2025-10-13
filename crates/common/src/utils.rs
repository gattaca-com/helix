use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    panic,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http::HeaderMap;
use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use reqwest::Url;
use tracing::error;
use tracing_appender::{non_blocking::WorkerGuard, rolling::Rotation};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use crate::LoggingConfig;

// Async because OTEL exporter spawns a task
pub async fn init_tracing_log(
    config: &LoggingConfig,
    region: &str,
    instance_id: String,
) -> WorkerGuard {
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_thread_ids(false)
        .with_target(true)
        .compact();

    let log_level = std::env::var("RUST_LOG")
        .map(|lev| lev.parse().expect("invalid RUST_LOG, change to eg 'info'"))
        .unwrap_or(tracing::Level::INFO);

    match config {
        LoggingConfig::Console => {
            let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
            let layer = tracing_subscriber::fmt::layer()
                .event_format(format.clone())
                .with_writer(writer)
                .with_filter(get_crate_filter(log_level));

            tracing_subscriber::registry().with(layer).init();
            guard
        }

        LoggingConfig::File { dir_path, file_name, otlp_server } => {
            let file_appender = tracing_appender::rolling::Builder::new()
                .filename_prefix(file_name)
                .max_log_files(14)
                .rotation(Rotation::DAILY)
                .build(dir_path)
                .expect("failed to create file log appender");

            let (writer, guard) = tracing_appender::non_blocking(file_appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .event_format(format)
                .with_writer(writer)
                .with_filter(get_crate_filter(log_level));

            match otlp_server {
                Some(exporter_url) => {
                    let exporter = opentelemetry_otlp::SpanExporter::builder()
                        .with_tonic()
                        .with_endpoint(exporter_url.to_string())
                        .build()
                        .unwrap();

                    let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                        .with_batch_exporter(exporter)
                        .with_resource(
                            Resource::builder()
                                .with_attribute(KeyValue::new("service.name", "helix_relay"))
                                .with_attribute(KeyValue::new("service.region", region.to_string()))
                                .with_attribute(KeyValue::new("service.instance_id", instance_id))
                                .build(),
                        )
                        .build()
                        .tracer("helix_relay");

                    let otel_layer = OpenTelemetryLayer::new(tracer)
                        .with_location(false)
                        .with_tracked_inactivity(false)
                        .with_threads(false)
                        .with_filter(get_crate_filter(tracing::Level::TRACE));

                    tracing_subscriber::registry().with(file_layer).with(otel_layer).init();
                }
                None => {
                    tracing_subscriber::registry().with(file_layer).init();
                }
            }

            guard
        }
    }
}

const CRATES: &[&str] =
    &["api", "beacon", "common", "database", "housekeeper", "network", "types", "website"];

fn get_crate_filter(crates_level: tracing::Level) -> EnvFilter {
    let mut env_filter = EnvFilter::new("info");

    for crate_name in CRATES {
        env_filter =
            env_filter.add_directive(format!("helix_{crate_name}={crates_level}").parse().unwrap())
    }

    // env_filter =
    // env_filter.add_directive(format!("helix_api::auctioneer=trace").parse().unwrap());

    env_filter
}

static APP_ID: OnceLock<String> = OnceLock::new();
static DISCORD_WEBHOOK_URL: OnceLock<Url> = OnceLock::new();

pub fn init_panic_hook(
    app_id: String,
    discord_web_hook: Option<Url>,
    crash_log_path: Option<PathBuf>,
) {
    APP_ID.set(app_id).unwrap();

    if let Some(webhook_url) = discord_web_hook {
        DISCORD_WEBHOOK_URL.set(webhook_url).unwrap();
    }

    panic::set_hook(Box::new(move |info| {
        let backtrace = backtrace::Backtrace::new();
        let crash_log = format!("Panic: {info}\nFull backtrace:\n{backtrace:?}\n");

        error!("{crash_log}");
        eprintln!("{crash_log}");

        alert_discord(&crash_log);

        if let Some(crash_log_path) = crash_log_path.clone() {
            save_to_file(crash_log_path, crash_log.clone());
        }
    }));
}

pub fn alert_discord(message: &str) {
    let Some(webhook_url) = DISCORD_WEBHOOK_URL.get() else {
        error!("discord hook not set!");
        error!("{message}");
        return;
    };

    let app_id = APP_ID.get().map(String::as_str).unwrap_or("unknown");

    let max_len = 1850.min(message.len());
    let msg = format!("Instance: RELAY-{app_id}\n{}", &message[..max_len]);

    let content = HashMap::from([("content", msg)]);

    if let Err(err) =
        reqwest::blocking::Client::new().post(webhook_url.clone()).json(&content).send()
    {
        error!("failed to send discord alert: {err}");
        eprintln!("failed to send discord alert: {err}");
    }
}

pub fn save_to_file(path: PathBuf, json: String) {
    // Create the directory if it doesn't exist
    if let Some(parent_dir) = Path::new(&path).parent() {
        fs::create_dir_all(parent_dir).expect("Failed to create directory");
    }

    // Open the file, truncating it if it already exists
    let mut file = File::create(&path).expect("Failed to create file");

    // Write the JSON string to the file
    file.write_all(json.as_bytes()).expect("Failed to write JSON to file");
}

// Returns request id from header if exists otherwise returns a random one
pub fn extract_request_id(headers: &HeaderMap) -> Uuid {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
        .unwrap_or(Uuid::new_v4())
}

////// TIME //////

/// Duration since UNIX_EPOCH
pub fn utcnow_dur() -> Duration {
    // safe since we're past UNIX_EPOCH
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
}

/// Seconds
pub fn utcnow_sec() -> u64 {
    utcnow_dur().as_secs()
}
/// Millis
pub fn utcnow_ms() -> u64 {
    utcnow_dur().as_millis() as u64
}
/// Micros
pub fn utcnow_us() -> u64 {
    utcnow_dur().as_micros() as u64
}
/// Nanos
pub fn utcnow_ns() -> u64 {
    utcnow_dur().as_nanos() as u64
}

pub fn avg_duration(duration: Duration, count: u32) -> Option<Duration> {
    if count != 0 { Some(duration / count) } else { None }
}

pub fn pin_thread_to_core(core: usize) -> bool {
    core_affinity::set_for_current(core_affinity::CoreId { id: core })
}
