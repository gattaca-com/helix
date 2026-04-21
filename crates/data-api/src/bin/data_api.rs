use std::{fs::File, sync::Arc};

use clap::Parser;
use helix_common::{
    local_cache::LocalCache,
    utils::{init_panic_hook, init_tracing_log},
};
use helix_data_api::{config::DataApiConfig, service::run_data_api};
use helix_database::PostgresDatabaseService;

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = "config.yml")]
    config: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let file = File::open(&args.config)
        .unwrap_or_else(|_| panic!("unable to find config file: '{}'", args.config));
    let config: DataApiConfig = serde_yaml::from_reader(file).expect("failed to parse config file");

    let _guard =
        init_tracing_log(&config.logging, &config.postgres.region_name, "helix-data".to_string())
            .await;

    init_panic_hook(
        "helix-data".to_string(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );

    let local_cache = Arc::new(LocalCache::new());
    let db = PostgresDatabaseService::from_postgres_config(&config.postgres, local_cache).await;
    db.init_forever().await;

    let db = Arc::new(db);
    let validator_preferences = Arc::new(config.validator_preferences);

    run_data_api(db, validator_preferences, config.api_port, config.router_config).await
}
