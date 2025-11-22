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

const CRATES: &[&str] = &["common", "relay", "simulator", "types"];

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_utcnow_functions_are_consistent() {
        let dur = utcnow_dur();
        let sec = utcnow_sec();
        let ms = utcnow_ms();
        let us = utcnow_us();
        let ns = utcnow_ns();

        // Check that conversions are consistent
        assert!(sec > 0);
        assert!(ms >= sec * 1000);
        assert!(us >= ms * 1000);
        assert!(ns >= us * 1000);

        // Check duration matches seconds (with some tolerance for timing)
        let dur_secs = dur.as_secs();
        assert!((dur_secs as i64 - sec as i64).abs() <= 1);
    }

    #[test]
    fn test_utcnow_functions_increase() {
        let ms1 = utcnow_ms();
        thread::sleep(Duration::from_millis(10));
        let ms2 = utcnow_ms();

        assert!(ms2 > ms1);
        assert!(ms2 - ms1 >= 10);
    }

    #[test]
    fn test_utcnow_sec() {
        let sec = utcnow_sec();
        // Should be after year 2020 (timestamp > 1_577_836_800)
        assert!(sec > 1_577_836_800);
    }

    #[test]
    fn test_utcnow_ms() {
        let ms = utcnow_ms();
        let sec = utcnow_sec();
        // Milliseconds should be much larger than seconds
        assert!(ms > sec * 1000);
    }

    #[test]
    fn test_utcnow_us() {
        let us = utcnow_us();
        let ms = utcnow_ms();
        // Microseconds should be larger than milliseconds * 1000
        assert!(us >= ms * 1000);
    }

    #[test]
    fn test_utcnow_ns() {
        let ns = utcnow_ns();
        let us = utcnow_us();
        // Nanoseconds should be larger than microseconds * 1000
        assert!(ns >= us * 1000);
    }

    #[test]
    fn test_avg_duration_normal_case() {
        let duration = Duration::from_secs(100);
        let count = 10;
        let avg = avg_duration(duration, count);

        assert!(avg.is_some());
        assert_eq!(avg.unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn test_avg_duration_zero_count() {
        let duration = Duration::from_secs(100);
        let count = 0;
        let avg = avg_duration(duration, count);

        assert!(avg.is_none());
    }

    #[test]
    fn test_avg_duration_one_count() {
        let duration = Duration::from_secs(42);
        let count = 1;
        let avg = avg_duration(duration, count);

        assert!(avg.is_some());
        assert_eq!(avg.unwrap(), Duration::from_secs(42));
    }

    #[test]
    fn test_avg_duration_with_milliseconds() {
        let duration = Duration::from_millis(1000);
        let count = 4;
        let avg = avg_duration(duration, count);

        assert!(avg.is_some());
        assert_eq!(avg.unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn test_extract_request_id_with_valid_header() {
        let mut headers = HeaderMap::new();
        let uuid = Uuid::new_v4();
        headers.insert("x-request-id", uuid.to_string().parse().unwrap());

        let extracted = extract_request_id(&headers);
        assert_eq!(extracted, uuid);
    }

    #[test]
    fn test_extract_request_id_without_header() {
        let headers = HeaderMap::new();
        let extracted = extract_request_id(&headers);

        // Should return a valid UUID (version 4)
        assert!(extracted.get_version_num() == 4);
    }

    #[test]
    fn test_extract_request_id_with_invalid_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "invalid-uuid".parse().unwrap());

        let extracted = extract_request_id(&headers);
        // Should return a new random UUID when parsing fails
        assert!(extracted.get_version_num() == 4);
    }

    #[test]
    fn test_extract_request_id_generates_different_uuids() {
        let headers = HeaderMap::new();
        let uuid1 = extract_request_id(&headers);
        let uuid2 = extract_request_id(&headers);

        // Should generate different UUIDs on each call
        assert_ne!(uuid1, uuid2);
    }

    #[test]
    fn test_save_to_file() {
        use std::fs;
        use std::path::PathBuf;

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_save_{}.txt", Uuid::new_v4()));
        let content = "test content".to_string();

        save_to_file(test_file.clone(), content.clone());

        // Verify file was created and contains correct content
        let read_content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(read_content, content);

        // Cleanup
        fs::remove_file(test_file).ok();
    }

    #[test]
    fn test_save_to_file_creates_directory() {
        use std::fs;
        use std::path::PathBuf;

        let temp_dir = std::env::temp_dir();
        let test_dir = temp_dir.join(format!("test_dir_{}", Uuid::new_v4()));
        let test_file = test_dir.join("nested").join("test.txt");
        let content = "nested content".to_string();

        save_to_file(test_file.clone(), content.clone());

        // Verify file was created in nested directory
        assert!(test_file.exists());
        let read_content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(read_content, content);

        // Cleanup
        fs::remove_dir_all(test_dir).ok();
    }

    #[test]
    fn test_save_to_file_truncates_existing() {
        use std::fs;

        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("test_truncate_{}.txt", Uuid::new_v4()));

        // Write initial content
        save_to_file(test_file.clone(), "initial content".to_string());

        // Write new content (should truncate)
        save_to_file(test_file.clone(), "new".to_string());

        // Verify file was truncated
        let read_content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(read_content, "new");
        assert_eq!(read_content.len(), 3);

        // Cleanup
        fs::remove_file(test_file).ok();
    }
}
