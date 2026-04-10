use std::{fs::File, sync::Arc};

use clap::Parser;
use helix_common::local_cache::LocalCache;
use helix_data_api::{config::DataApiConfig, service::run_data_api};
use helix_database::PostgresDatabaseService;

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = "config.yml")]
    config: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let file = File::open(&args.config)
        .unwrap_or_else(|_| panic!("unable to find config file: '{}'", args.config));
    let config: DataApiConfig = serde_yaml::from_reader(file).expect("failed to parse config file");

    let local_cache = Arc::new(LocalCache::new());
    let db =
        PostgresDatabaseService::from_postgres_config(&config.postgres, local_cache).await;
    db.init_forever().await;

    let db = Arc::new(db);
    let validator_preferences = Arc::new(config.validator_preferences);

    run_data_api(db, validator_preferences, config.api_port).await
}
