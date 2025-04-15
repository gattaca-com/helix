use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    panic,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use helix_types::Slot;
use http::HeaderMap;
use reqwest::Url;
use tracing::error;
use tracing_appender::{non_blocking::WorkerGuard, rolling::Rotation};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};
use uuid::Uuid;

use crate::LoggingConfig;

pub fn get_payload_attributes_key(parent_hash: &B256, slot: Slot) -> String {
    format!("{parent_hash:?}:{slot}")
}

pub fn init_tracing_log(config: &LoggingConfig) -> WorkerGuard {
    let format =
        tracing_subscriber::fmt::format().with_level(true).with_thread_ids(false).with_target(true);

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

        LoggingConfig::File { dir_path, file_name } => {
            let file_appender = tracing_appender::rolling::Builder::new()
                .filename_prefix(file_name)
                .max_log_files(14)
                .rotation(Rotation::DAILY)
                .build(dir_path)
                .expect("failed to create file log appender");

            let (writer, guard) = tracing_appender::non_blocking(file_appender);
            let layer = tracing_subscriber::fmt::layer()
                .event_format(format)
                .with_writer(writer)
                .with_filter(get_crate_filter(log_level));

            tracing_subscriber::registry().with(layer).init();
            guard
        }
    }
}

const CRATES: &[&str] =
    &["api", "beacon", "common", "database", "datastore", "housekeeper", "types", "website"];

fn get_crate_filter(crates_level: tracing::Level) -> EnvFilter {
    let mut env_filter = EnvFilter::new("info");

    for crate_name in CRATES {
        env_filter =
            env_filter.add_directive(format!("helix_{crate_name}={crates_level}").parse().unwrap())
    }

    env_filter
}

pub fn init_panic_hook(
    instance_id: String,
    discord_web_hook: Option<Url>,
    crash_log_path: Option<PathBuf>,
) {
    panic::set_hook(Box::new(move |info| {
        let backtrace = backtrace::Backtrace::new();
        let crash_log = format!(
            "Panic: {info}\nFull backtrace:\n{backtrace:?}\n",
            info = info,
            backtrace = backtrace
        );

        error!("{crash_log}");
        eprintln!("{crash_log}");

        if let Some(crash_log_path) = crash_log_path.clone() {
            save_to_file(crash_log_path, crash_log.clone());
        }
        if let Some(discord_web_hook) = discord_web_hook.clone() {
            alert_discord(
                discord_web_hook,
                &format!("Relay: {} crashed! Please see the console log for details!", instance_id),
                &instance_id,
            );
        }
    }));
}

pub fn alert_discord(webhook_url: Url, message: &str, region: &str) {
    let max_length = message.len().min(1850);
    let content = format!("Instance: RELAY-{}\n{}", region, &message[..max_length]);

    let mut payload = HashMap::new();
    payload.insert("content", content);

    if let Err(err) = reqwest::blocking::Client::new().post(webhook_url).json(&payload).send() {
        error!(?err, message, "could not send alert to Discord");
        eprintln!("could not send alert to Discord: err={err}, message={message}");
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

/// Seconds
pub fn utcnow_sec() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
/// Millis
pub fn utcnow_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}
/// Micros
pub fn utcnow_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}
/// Nanos
pub fn utcnow_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}
