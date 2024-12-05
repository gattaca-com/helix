use helix_api::service::ApiService;
use helix_common::{metrics::start_metrics_server, LoggingConfig, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_utils::set_panic_hook;
use helix_website::website_service::WebsiteService;

use tikv_jemallocator::Jemalloc;
use tokio::runtime::Builder;
use tracing_appender::rolling::Rotation;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

async fn run() {
    let config = match RelayConfig::load() {
        Ok(config) => config,
        Err(e) => {
            println!("Failed to load config: {:?}", e);
            std::process::exit(1);
        }
    };

    let _guard;

    match &config.logging {
        LoggingConfig::Console => {
            set_panic_hook(
                config.postgres.region_name.clone(),
                config.discord_webhook_url.clone(),
                None,
            );
            let filter_layer = tracing_subscriber::EnvFilter::from_default_env();

            tracing_subscriber::fmt().with_env_filter(filter_layer).init();
        }
        LoggingConfig::File { dir_path, file_name } => {
            set_panic_hook(
                config.postgres.region_name.clone(),
                config.discord_webhook_url.clone(),
                Some(format!("{}/crash.log", dir_path.clone())),
            );
            let file_appender = tracing_appender::rolling::Builder::new()
                .filename_prefix(file_name)
                .max_log_files(14)
                .rotation(Rotation::DAILY)
                .build(dir_path)
                .expect("failed to create log appender!");

            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            let filter_layer = tracing_subscriber::EnvFilter::from_default_env();

            tracing_subscriber::fmt()
                .with_env_filter(filter_layer)
                .with_writer(non_blocking)
                .init();
            _guard = Some(guard);
        }
    }

    let mut handles = Vec::new();

    let postgres_db = PostgresDatabaseService::from_relay_config(&config).await;
    start_metrics_server();

    // Try to run database migrations until they succeed
    loop {
        match postgres_db.run_migrations().await {
            Ok(_) => break,
            Err(e) => {
                tracing::error!("Failed to run migrations: {:?}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }

    // Start the API service
    handles.push(tokio::spawn(ApiService::run(config.clone(), postgres_db.clone())));

    // Start the website service (if enabled)
    if config.website.enabled {
        handles.push(tokio::spawn(WebsiteService::run_loop(config.clone(), postgres_db.clone())));
    }

    futures::future::join_all(handles).await;
}

fn main() {
    let num_cpus = num_cpus::get();
    let worker_threads = num_cpus;
    let max_blocking_threads = num_cpus / 2;

    let rt = Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(max_blocking_threads)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(run());
}
