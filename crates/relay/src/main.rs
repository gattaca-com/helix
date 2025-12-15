use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use eyre::eyre;
use helix_common::{
    RelayConfig,
    api_provider::DefaultApiProvider,
    load_config, load_keypair,
    local_cache::LocalCache,
    metrics::start_metrics_server,
    signing::RelaySigningContext,
    task::{block_on, init_runtime},
    utils::{init_panic_hook, init_tracing_log},
};
use helix_relay::{
    Api, DefaultBidAdjustor, PostgresDatabaseService, RelayNetworkManager, WebsiteService,
    start_admin_service, start_api_service, start_beacon_client, start_db_service,
    start_housekeeper,
};
use helix_types::BlsKeypair;
use tikv_jemallocator::Jemalloc;
use tokio::signal::unix::SignalKind;
use tracing::{error, info};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Clone)]
struct ApiProd;

impl Api for ApiProd {
    type ApiProvider = DefaultApiProvider;
}

fn main() {
    let config = load_config();
    init_runtime(&config);

    let keypair = load_keypair();

    let instance_id = config
        .instance_id
        .clone()
        .unwrap_or_else(|| format!("RelayUnknown_{}", config.postgres.region_name));

    let _guard = block_on(init_tracing_log(
        &config.logging,
        &config.postgres.region_name,
        instance_id.clone(),
    ));

    init_panic_hook(
        config.postgres.region_name.clone(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );

    block_on(start_metrics_server(&config));
    match block_on(run(instance_id, config, keypair)) {
        Ok(_) => info!("relay exited"),
        Err(err) => {
            error!(%err, "relay exited with error");
            panic!("relay exited with error: {err}");
        }
    }
}

async fn run(instance_id: String, config: RelayConfig, keypair: BlsKeypair) -> eyre::Result<()> {
    let beacon_client = start_beacon_client(&config);
    let chain_info = beacon_client.load_chain_info().await;
    let chain_info = Arc::new(chain_info);

    info!(
        instance_id,
        region = config.postgres.region_name,
        network =% chain_info.name,
        pubkey =% keypair.pk,
        "starting relay"
    );

    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));

    let known_validators_loaded = Arc::new(AtomicBool::default());

    let db = start_db_service(&config, known_validators_loaded.clone()).await?;
    let local_cache = start_auctioneer(db.clone()).await?;

    let event_channel = crossbeam_channel::bounded(10_000);
    let relay_network_api =
        RelayNetworkManager::new(config.relay_network.clone(), relay_signing_context.clone());

    let (top_bid_tx, _) = tokio::sync::broadcast::channel(100);
    let (getheader_call_tx, _) = tokio::sync::broadcast::channel(100);

    config.router_config.validate_bid_sorter()?;

    let current_slot_info = start_housekeeper(
        db.clone(),
        local_cache.clone(),
        &config,
        beacon_client.clone(),
        chain_info.clone(),
        event_channel.0.clone(),
        relay_network_api.clone(),
    )
    .await
    .map_err(|e| eyre!("housekeeper init: {e}"))?;

    let terminating = Arc::new(AtomicBool::default());

    start_admin_service(local_cache.clone(), &config);

    tokio::spawn(start_api_service::<ApiProd>(
        config.clone(),
        db.clone(),
        local_cache,
        current_slot_info,
        chain_info,
        relay_signing_context,
        beacon_client,
        Arc::new(DefaultApiProvider {}),
        DefaultBidAdjustor {},
        known_validators_loaded,
        terminating.clone(),
        top_bid_tx,
        getheader_call_tx,
        event_channel,
        relay_network_api.api(),
    ));

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

pub async fn start_auctioneer(db: Arc<PostgresDatabaseService>) -> eyre::Result<Arc<LocalCache>> {
    let auctioneer = Arc::new(LocalCache::new());
    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");
        auctioneer_clone.update_builder_infos(&builder_infos, true);
    });

    Ok(auctioneer)
}
