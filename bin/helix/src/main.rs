use std::sync::Arc;

use eyre::eyre;
use helix_api::start_api_service;
use helix_beacon::start_beacon_client;
use helix_common::{
    load_config, load_keypair,
    metadata_provider::DefaultMetadataProvider,
    metrics::start_metrics_server,
    signing::RelaySigningContext,
    utils::{init_panic_hook, init_tracing_log},
    RelayConfig,
};
use helix_database::start_db_service;
use helix_datastore::start_auctioneer;
use helix_housekeeper::start_housekeeper;
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
    start_metrics_server();

    info!(
        region = config.postgres.region_name,
        network =% config.network_config,
        pubkey =% keypair.pk,
        "starting relay"
    );

    match run(config, keypair).await {
        Ok(_) => info!("relay exited"),
        Err(err) => {
            error!(%err, "relay exited with error");
            panic!("relay exited with error: {err}");
        }
    }
}

async fn run(config: RelayConfig, keypair: BlsKeypair) -> eyre::Result<()> {
    let chain_info = Arc::new(config.network_config.to_chain_info());
    let relay_signing_context =
        Arc::new(RelaySigningContext { keypair, context: chain_info.clone() });

    let beacon_client = start_beacon_client(&config);
    let db = start_db_service(&config).await?;
    let auctioneer = start_auctioneer(&config, &db).await?;

    let current_slot_info = start_housekeeper(
        db.clone(),
        auctioneer.clone(),
        &config,
        beacon_client.clone(),
        chain_info.clone(),
    )
    .await
    .map_err(|e| eyre!("housekeeper init: {e}"))?;

    start_api_service(
        config.clone(),
        db.clone(),
        auctioneer,
        chain_info,
        relay_signing_context,
        beacon_client,
        Arc::new(DefaultMetadataProvider {}),
        current_slot_info,
    );

    if config.website.enabled {
        tokio::spawn(WebsiteService::run_loop(config, db));
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
