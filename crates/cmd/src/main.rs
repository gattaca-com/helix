use helix_api::service::ApiService;
use helix_common::{LoggingConfig, RelayConfig};
use tokio::runtime::Builder;

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
            tracing_subscriber::fmt().init();
        }
        LoggingConfig::File { dir_path, file_name } => {
            let file_appender = tracing_appender::rolling::daily(dir_path, file_name);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            let filter_layer = tracing_subscriber::EnvFilter::from_default_env();

            tracing_subscriber::fmt().with_env_filter(filter_layer).with_writer(non_blocking).init();
            _guard = Some(guard);
        }
    }

    ApiService::run(config).await;
}

fn main() {
    let num_cpus = num_cpus::get();
    let worker_threads = num_cpus;
    let max_blocking_threads = num_cpus / 2;

    let rt = Builder::new_multi_thread().worker_threads(worker_threads).max_blocking_threads(max_blocking_threads).enable_all().build().unwrap();

    rt.block_on(run());
}
