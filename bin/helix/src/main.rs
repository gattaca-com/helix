use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use eyre::eyre;
use helix_api::{start_admin_service, start_api_service, Api};
use helix_beacon::start_beacon_client;
use helix_common::{
    bid_sorter::{start_bid_sorter, BestGetHeader, FloorBid},
    load_config, load_keypair,
    metadata_provider::DefaultMetadataProvider,
    metrics::start_metrics_server,
    signing::RelaySigningContext,
    utils::{init_panic_hook, init_tracing_log},
    RelayConfig,
};
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, start_db_service};
use helix_datastore::{local::local_cache::LocalCache, start_auctioneer};
use helix_housekeeper::start_housekeeper;
use helix_types::BlsKeypair;
use helix_website::website_service::WebsiteService;
use tikv_jemallocator::Jemalloc;
use tokio::signal::unix::SignalKind;
use tracing::{error, info};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Clone)]
struct ApiProd;

impl Api for ApiProd {
    type Auctioneer = LocalCache;
    type DatabaseService = PostgresDatabaseService;
    type MetadataProvider = DefaultMetadataProvider;
}

#[tokio::main]
async fn main() {
    let config = load_config();
    let keypair = load_keypair();

    let instance_id = config.instance_id.clone().unwrap_or_else(|| {
        format!(
            "RelayUnknown_{}_{}",
            config.network_config.short_name(),
            config.postgres.region_name
        )
    });
    let _guard =
        init_tracing_log(&config.logging, &config.postgres.region_name, instance_id.clone());

    init_panic_hook(
        config.postgres.region_name.clone(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );
    start_metrics_server(&config);

    info!(
        instance_id,
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

    let (sorter_tx, sorter_rx) = crossbeam_channel::bounded(10_000);

    let known_validators_loaded = Arc::new(AtomicBool::default());

    let beacon_client = start_beacon_client(&config);
    let db = start_db_service(&config, known_validators_loaded.clone()).await?;
    let auctioneer = start_auctioneer(sorter_tx.clone(), db.clone()).await?;

    let (top_bid_tx, _) = tokio::sync::broadcast::channel(100);
    let shared_best_header = BestGetHeader::new();
    let shared_floor_bid = FloorBid::new();

    if config.router_config.validate_bid_sorter()? {
        start_bid_sorter(
            sorter_rx,
            top_bid_tx.clone(),
            shared_best_header.clone(),
            shared_floor_bid.clone(),
        );
    }

    let current_slot_info = start_housekeeper(
        db.clone(),
        auctioneer.clone(),
        &config,
        beacon_client.clone(),
        chain_info.clone(),
        sorter_tx.clone(),
    )
    .await
    .map_err(|e| eyre!("housekeeper init: {e}"))?;

    let terminating = Arc::new(AtomicBool::default());

    start_admin_service(auctioneer.clone(), &config);

    start_api_service::<ApiProd>(
        config.clone(),
        db.clone(),
        auctioneer,
        chain_info,
        relay_signing_context,
        beacon_client,
        Arc::new(DefaultMetadataProvider {}),
        current_slot_info,
        known_validators_loaded,
        terminating.clone(),
        sorter_tx,
        top_bid_tx,
        shared_best_header,
        shared_floor_bid,
    );

    let termination_grace_period = config.router_config.shutdown_delay_ms;

    if config.website.enabled {
        tokio::spawn(WebsiteService::run_loop(config, db));
    }

    // wait for SIGTERM or SIGINT
    let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    // Set terminating flag.
    terminating.store(true, Ordering::Relaxed);
    if termination_grace_period != 0 {
        // Wait for the grace period to expire before exiting.
        tracing::info!("Pausing for {termination_grace_period}ms before exit");
        tokio::time::sleep(Duration::from_millis(termination_grace_period)).await;
    }

    Ok(())
}
