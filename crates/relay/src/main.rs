use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use eyre::eyre;
use flux::{
    spine::FluxSpine,
    tile::{TileConfig, TileName, attach_tile},
    utils::ThreadPriority,
};
use flux_utils::SharedVector;
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
    Api, Auctioneer, AuctioneerHandle, BidSorter, BidSubmissionTcpListener, DbHandle, DecoderTile,
    DefaultBidAdjustor, FutureBidSubmissionResult, HelixSpine, InternalBidSubmission, RegWorker,
    RegWorkerHandle, RelayNetworkManager, S3PayloadSaver, SubmissionDataWithSpan, WebsiteService,
    spawn_tokio_monitoring, start_admin_service, start_api_service, start_beacon_client,
    start_db_service, start_housekeeper,
};
use helix_types::BlsKeypair;
use tikv_jemallocator::Jemalloc;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

const ADMIN_TOKEN_ENV_VAR: &str = "ADMIN_TOKEN";

const MAX_SUBMISSIONS_PER_SLOT: usize = 10_000;

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

    let app_id = config
        .instance_id
        .clone()
        .unwrap_or_else(|| format!("RELAY-{}", config.postgres.region_name));

    init_panic_hook(app_id, config.discord_webhook_url.clone(), config.logging.dir_path());

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
    let local_cache = Arc::new(LocalCache::new());

    let (db_request_sender, db_request_receiver) = crossbeam_channel::bounded(10_000);
    let (db_batch_request_sender, db_batch_request_receiver) = crossbeam_channel::bounded(10_000);
    let db = start_db_service(
        &config,
        known_validators_loaded.clone(),
        db_request_receiver,
        db_batch_request_receiver,
        local_cache.clone(),
    )
    .await?;
    let db_handle = DbHandle::new(db_request_sender, db_batch_request_sender);
    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));

    let relay_network_api =
        RelayNetworkManager::new(config.relay_network.clone(), relay_signing_context.clone());

    config.router_config.validate_bid_sorter()?;

    let (reg_worker_tx, reg_worker_rx) = crossbeam_channel::bounded(100_000);

    let (event_tx, event_rx) = crossbeam_channel::bounded(10_000);

    let current_slot_info = start_housekeeper(
        db_handle.clone(),
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
    let termination_grace_period = Duration::from_millis(config.router_config.shutdown_delay_ms);

    spawn_tokio_monitoring();

    let spine = HelixSpine::new(None);
    spine.start(None, Some(termination_grace_period), |spine| {
        start_admin_service(local_cache.clone(), expect_env_var(ADMIN_TOKEN_ENV_VAR));

        let auctioneer_handle = AuctioneerHandle::new(event_tx.clone());
        let registrations_handle = RegWorkerHandle::new(reg_worker_tx);

        let (top_bid_tx, _) = tokio::sync::broadcast::channel(100);

        let submissions = Arc::new(SharedVector::<InternalBidSubmission>::with_capacity(
            MAX_SUBMISSIONS_PER_SLOT,
        ));
        let future_results = Arc::new(SharedVector::<FutureBidSubmissionResult>::with_capacity(
            MAX_SUBMISSIONS_PER_SLOT,
        ));
        let decoded = Arc::new(SharedVector::<SubmissionDataWithSpan>::with_capacity(
            MAX_SUBMISSIONS_PER_SLOT,
        ));

        let bid_producer = spine.spine.standalone_producer_for(TileName::from_str_truncate("Api"));
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
            db_handle.clone(),
            auctioneer_handle.clone(),
            registrations_handle,
            submissions.clone(),
            bid_producer,
            future_results.clone(),
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
            // TODO multiple decoder tiles
            let decoder_tile = DecoderTile::new(
                local_cache.as_ref().clone(),
                chain_info.as_ref().clone(),
                config.clone(),
                submissions.clone(),
                future_results.clone(),
                decoded.clone(),
            );
            attach_tile(
                decoder_tile,
                spine,
                TileConfig::new(config.cores.decoder, ThreadPriority::OSDefault),
            );

            let raw_payloads_tx =
                config.s3_config.clone().map(|cfg| S3PayloadSaver::new(cfg).spawn());

            let sock_addr =
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.tcp_port));
            let block_submission_tcp_listener = BidSubmissionTcpListener::new(
                sock_addr,
                local_cache.api_key_cache.clone(),
                config.tcp_max_connections,
                raw_payloads_tx,
                submissions,
            );
            attach_tile(
                block_submission_tcp_listener,
                spine,
                TileConfig::new(
                    config.cores.tcp_bid_submissions_tile,
                    flux::utils::ThreadPriority::High,
                ),
            );

            let auctioneer_core = config.cores.auctioneer;
            let auctioneer = Auctioneer::new(
                chain_info.as_ref().clone(),
                config,
                db_handle.clone(),
                BidSorter::new(top_bid_tx),
                local_cache.as_ref().clone(),
                DefaultBidAdjustor {},
                event_tx,
                event_rx,
                auctioneer_core,
                future_results,
                decoded,
                auctioneer_handle.clone(),
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

    if !termination_grace_period.is_zero() {
        tracing::info!("Pausing for {}ms before exit", termination_grace_period.as_millis());

        tokio::time::sleep(termination_grace_period).await;
    }

    Ok(())
}
