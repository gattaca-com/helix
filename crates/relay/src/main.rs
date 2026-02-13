use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use eyre::eyre;
use flux::{
    tile::{TileConfig, attach_tile},
    utils::ThreadPriority,
};
use helix_common::{
    RelayConfig,
    api_provider::DefaultApiProvider,
    expect_env_var, load_config, load_keypair,
    local_cache::LocalCache,
    metrics::start_metrics_server,
    signing::RelaySigningContext,
    task::{block_on, init_runtime},
    utils::{init_panic_hook, init_tracing_log},
};
use helix_relay::{
    Api, Auctioneer, AuctioneerHandle, BidSorter, DefaultBidAdjustor, HelixSpine,
    PostgresDatabaseService, RegWorker, RegWorkerHandle, RelayNetworkManager, SubWorker,
    WebsiteService, start_admin_service, start_api_service, start_beacon_client, start_db_service,
    start_housekeeper,
};
use helix_types::BlsKeypair;
use tikv_jemallocator::Jemalloc;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

const ADMIN_TOKEN_ENV_VAR: &str = "ADMIN_TOKEN";

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
    let chain_info = Arc::new(beacon_client.load_chain_info().await);

    info!(
        instance_id,
        region = config.postgres.region_name,
        network =% chain_info.name,
        pubkey =% keypair.pk,
        "starting relay"
    );

    let known_validators_loaded = Arc::new(AtomicBool::default());
    let db = start_db_service(&config, known_validators_loaded.clone()).await?;
    let local_cache = start_local_cache_reloader(db.clone()).await?;

    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));

    let relay_network_api =
        RelayNetworkManager::new(config.relay_network.clone(), relay_signing_context.clone());

    config.router_config.validate_bid_sorter()?;

    let (sub_worker_tx, sub_worker_rx) = crossbeam_channel::bounded(10_000);
    let (reg_worker_tx, reg_worker_rx) = crossbeam_channel::bounded(100_000);

    let (event_tx, event_rx) = crossbeam_channel::bounded(10_000);

    let current_slot_info = start_housekeeper(
        db.clone(),
        local_cache.clone(),
        &config,
        beacon_client.clone(),
        chain_info.clone(),
        event_tx.clone(),
        relay_network_api.clone(),
    )
    .await
    .map_err(|e| eyre!("housekeeper init: {e}"))?;

    let terminating = Arc::new(AtomicBool::default());
    let termination_grace_period = config.router_config.shutdown_delay_ms;

    let spine = HelixSpine::new(None);
    spine.start(None, |spine| {
        start_admin_service(local_cache.clone(), expect_env_var(ADMIN_TOKEN_ENV_VAR));

        let auctioneer_handle = AuctioneerHandle::new(sub_worker_tx, event_tx.clone());
        let registrations_handle = RegWorkerHandle::new(reg_worker_tx);

        let (top_bid_tx, _) = tokio::sync::broadcast::channel(100);

        start_api_service::<ApiProd>(
            config.clone(),
            db.clone(),
            local_cache.clone(),
            current_slot_info,
            chain_info.clone(),
            relay_signing_context,
            beacon_client,
            Arc::new(DefaultApiProvider {}),
            known_validators_loaded,
            terminating.clone(),
            top_bid_tx.clone(),
            relay_network_api.api(),
            auctioneer_handle,
            registrations_handle,
        );

        if config.website.enabled {
            tokio::spawn(WebsiteService::run_loop(config.clone(), db.clone()));
        }

        if config.is_registration_instance {
            for core in config.cores.reg_workers.clone() {
                let worker =
                    RegWorker::new(core, chain_info.as_ref().clone(), reg_worker_rx.clone());

                attach_tile(worker, spine, TileConfig::new(core, ThreadPriority::OSDefault));
            }
        }

        if config.is_submission_instance {
            for core in config.cores.sub_workers.clone() {
                let worker = SubWorker::new(
                    core,
                    event_tx.clone(),
                    sub_worker_rx.clone(),
                    local_cache.as_ref().clone(),
                    chain_info.as_ref().clone(),
                    config.clone(),
                );

                attach_tile(worker, spine, TileConfig::new(core, ThreadPriority::OSDefault));
            }

            let auctioneer_core = config.cores.auctioneer;
            let auctioneer = Auctioneer::new(
                chain_info.as_ref().clone(),
                config,
                db,
                BidSorter::new(top_bid_tx),
                local_cache.as_ref().clone(),
                DefaultBidAdjustor {},
                event_tx,
                event_rx,
                auctioneer_core,
            );
            attach_tile(
                auctioneer,
                spine,
                TileConfig::new(auctioneer_core, ThreadPriority::OSDefault),
            );
        }
    });

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    terminating.store(true, Ordering::Relaxed);

    if termination_grace_period != 0 {
        tracing::info!("Pausing for {termination_grace_period}ms before exit");

        tokio::time::sleep(Duration::from_millis(termination_grace_period)).await;
    }

    Ok(())
}

pub async fn start_local_cache_reloader(
    db: Arc<PostgresDatabaseService>,
) -> eyre::Result<Arc<LocalCache>> {
    let local_cache = Arc::new(LocalCache::new());
    let local_cache_clone = local_cache.clone();
    tokio::spawn(async move {
        let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");
        local_cache_clone.update_builder_infos(&builder_infos, true);
    });

    Ok(local_cache)
}
