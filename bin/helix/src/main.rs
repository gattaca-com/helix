use helix_api::service::ApiService;
use helix_common::{
    load_config, load_keypair,
    metrics::start_metrics_server,
    task::init_runtime,
    utils::{init_panic_hook, init_tracing_log},
    RelayConfig,
};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_types::BlsKeypair;
use helix_website::website_service::WebsiteService;
use tikv_jemallocator::Jemalloc;
use tokio::signal::unix::SignalKind;
use tracing::{error, info};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() {
    let config = load_config();
    let keypair = load_keypair();

    let _guard = init_tracing_log(&config.logging);

    init_panic_hook(
        config.postgres.region_name.clone(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );
    init_runtime();
    start_metrics_server();

    info!(network =% config.network_config, pubkey =% keypair.pk, "starting relay");

    match run(config, keypair).await {
        Ok(_) => info!("relay exited"),
        Err(err) => {
            error!(%err, "relay exited with error");
            panic!("relay exited with error: {err}");
        }
    }
}

async fn run(config: RelayConfig, keypair: BlsKeypair) -> eyre::Result<()> {
    let postgres_db = PostgresDatabaseService::from_relay_config(&config).await;
    postgres_db.init_forever().await;

    tokio::spawn(ApiService::run(config.clone(), postgres_db.clone(), keypair));

    // start the website service (if enabled)
    if config.website.enabled {
        tokio::spawn(WebsiteService::run_loop(config.clone(), postgres_db.clone()));
    }

    // wait for SIGTERM or SIGINT
    let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

    // TODO: here we should stop serving headers and sleep until slot is finished
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    Ok(())
}
