use helix_api::service::ApiService;
use helix_common::{LoggingConfig, RelayConfig};

#[tokio::main]
async fn main() {
    let config =
        match RelayConfig::load() {
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

            tracing_subscriber::fmt()
                .with_env_filter(filter_layer)
                .with_writer(non_blocking)
                .init();
            _guard = Some(guard);
        }
    }

    ApiService::run(config).await;
}
