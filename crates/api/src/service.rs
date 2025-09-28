use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use bytes::Bytes;
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, signing::RelaySigningContext, RelayConfig,
};
use helix_housekeeper::{chain_event_updater::SlotData, CurrentSlotInfo};
use helix_network::api::RelayNetworkApi;
use moka::sync::Cache;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{
    auctioneer::spawn_auctioneer,
    builder::api::BuilderApi,
    gossip::{self},
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::ProposerApi,
    relay_data::{BidsCache, DataApi, DeliveredPayloadsCache, SelectiveExpiry},
    router::build_router,
    Api,
};

pub(crate) const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SIMULATOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn run_api_service<A: Api>(
    mut config: RelayConfig,
    db: Arc<A::DatabaseService>,
    local_cache: Arc<LocalCache>,
    current_slot_info: CurrentSlotInfo,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<A::MetadataProvider>,
    known_validators_loaded: Arc<AtomicBool>,
    terminating: Arc<AtomicBool>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    slot_data_rx: crossbeam_channel::Receiver<SlotData>,
    relay_network_api: RelayNetworkApi,
) {
    let gossiper = Arc::new(
        GrpcGossiperClientManager::new(config.relays.iter().map(|cfg| cfg.url.clone()).collect())
            .await
            .expect("failed to initialise gRPC gossiper"),
    );

    let validator_preferences = Arc::new(config.validator_preferences.clone());

    let (gossip_sender, gossip_receiver) = tokio::sync::mpsc::channel(10_000);
    let (merge_pool_tx, pool_rx) = tokio::sync::mpsc::channel(10_000);
    let (merge_requests_tx, _merge_requests_rx) = mpsc::channel(10_000);

    // spawn auctioneer
    let handle = spawn_auctioneer::<A>(
        Arc::unwrap_or_clone(chain_info.clone()),
        config.clone(),
        tokio::runtime::Handle::current(),
        db.clone(),
        merge_pool_tx,
        Arc::unwrap_or_clone(local_cache.clone()),
        top_bid_tx.clone(),
        slot_data_rx,
    );

    let accept_optimistic = Arc::new(AtomicBool::new(true));
    let builder_api = BuilderApi::<A>::new(
        local_cache.clone(),
        db.clone(),
        chain_info.clone(),
        gossiper.clone(),
        config.clone(),
        current_slot_info.clone(),
        top_bid_tx,
        accept_optimistic,
        handle.clone(),
        metadata_provider.clone(),
    );
    let builder_api = Arc::new(builder_api);

    gossiper.start_server(gossip_sender).await;

    let proposer_api = Arc::new(ProposerApi::<A>::new(
        local_cache,
        db.clone(),
        gossiper.clone(),
        metadata_provider.clone(),
        relay_signing_context,
        multi_beacon_client,
        chain_info.clone(),
        validator_preferences.clone(),
        config.clone(),
        current_slot_info,
        merge_requests_tx,
        handle,
    ));

    tokio::spawn(gossip::process_gossip_messages(
        builder_api.clone(),
        proposer_api.clone(),
        gossip_receiver,
    ));

    if config.block_merging_config.is_enabled {
        tokio::spawn(proposer_api.clone().process_block_merging(pool_rx));
    }

    let data_api = Arc::new(DataApi::<A>::new(validator_preferences.clone(), db.clone()));

    let bids_cache: BidsCache =
        Cache::builder().time_to_idle(Duration::from_secs(300)).max_capacity(10_000).build();

    let delivered_payloads_cache: DeliveredPayloadsCache = Cache::builder()
        .expire_after(SelectiveExpiry)
        .time_to_idle(Duration::from_secs(300))
        .max_capacity(10_000)
        .build();

    let router = build_router(
        &mut config.router_config,
        builder_api,
        proposer_api,
        data_api,
        relay_network_api,
        bids_cache,
        delivered_payloads_cache,
        known_validators_loaded,
        terminating,
    );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4040").await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}
