use std::{fs::File, sync::Arc};

use clap::Parser;
use helix_admin::{config::AdminConfig, service::run_admin_api};
use helix_common::{
    config::expect_env_var,
    local_cache::LocalCache,
    utils::{init_panic_hook, init_tracing_log},
};
use helix_database::PostgresDatabaseService;

const ADMIN_TOKEN_ENV_VAR: &str = "ADMIN_TOKEN";
/// Optional override for the token used to call the relay's admin API;
/// defaults to ADMIN_TOKEN.
const RELAY_ADMIN_TOKEN_ENV_VAR: &str = "RELAY_ADMIN_TOKEN";

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = "admin-config.yml")]
    config: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let file = File::open(&args.config)
        .unwrap_or_else(|_| panic!("unable to find config file: '{}'", args.config));
    let config: AdminConfig = serde_yaml::from_reader(file).expect("failed to parse config file");

    let _guard =
        init_tracing_log(&config.logging, &config.postgres.region_name, "helix-admin".to_string())
            .await;

    init_panic_hook(
        "helix-admin".to_string(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );

    let admin_token = expect_env_var(ADMIN_TOKEN_ENV_VAR);
    let relay_admin_token =
        std::env::var(RELAY_ADMIN_TOKEN_ENV_VAR).unwrap_or_else(|_| admin_token.clone());

    let local_cache = Arc::new(LocalCache::new());
    let db = PostgresDatabaseService::from_postgres_config(&config.postgres, local_cache).await;
    db.init_forever().await;

    run_admin_api(Arc::new(db), config, admin_token, relay_admin_token).await
}
