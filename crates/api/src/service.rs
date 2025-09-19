use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use bytes::Bytes;
use helix_beacon::{
    beacon_client::BeaconClient, multi_beacon_client::MultiBeaconClient, BlockBroadcaster,
};
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, signing::RelaySigningContext,
    BroadcasterConfig, RelayConfig,
};
use helix_housekeeper::CurrentSlotInfo;
use moka::sync::Cache;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{
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
    auctioneer: Arc<LocalCache>,
    current_slot_info: CurrentSlotInfo,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<A::MetadataProvider>,
    known_validators_loaded: Arc<AtomicBool>,
    terminating: Arc<AtomicBool>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
) {
    let broadcasters = init_broadcasters(&config).await;

    let gossiper = Arc::new(
        GrpcGossiperClientManager::new(config.relays.iter().map(|cfg| cfg.url.clone()).collect())
            .await
            .expect("failed to initialise gRPC gossiper"),
    );

    let validator_preferences = Arc::new(config.validator_preferences.clone());

    let (gossip_sender, gossip_receiver) = tokio::sync::mpsc::channel(10_000);
    let (merge_pool_tx, pool_rx) = tokio::sync::mpsc::channel(10_000);

    let (merge_requests_tx, merge_requests_rx) = mpsc::channel(10_000);

    let accept_optimistic = Arc::new(AtomicBool::new(true));

    let builder_api = BuilderApi::<A>::new(
        auctioneer.clone(),
        db.clone(),
        chain_info.clone(),
        gossiper.clone(),
        config.clone(),
        current_slot_info.clone(),
        top_bid_tx,
        accept_optimistic,
    );
    let builder_api = Arc::new(builder_api);

    gossiper.start_server(gossip_sender).await;

    // let (v3_payload_request_send, v3_payload_request_recv) = mpsc::channel(32);
    // if let Some(v3_port) = config.v3_port {
    //     // v3 tcp optimistic configured
    //     tokio::spawn(builder::v3::tcp::run_api(v3_port, builder_api.clone()));
    // }

    // // Start builder block fetcher
    // tokio::spawn(builder::v3::payload::fetch_builder_blocks(
    //     builder_api.clone(),
    //     v3_payload_request_recv,
    //     relay_signing_context.clone(),
    // ));

    let proposer_api = Arc::new(ProposerApi::<A>::new(
        auctioneer.clone(),
        db.clone(),
        gossiper.clone(),
        metadata_provider.clone(),
        relay_signing_context,
        broadcasters,
        multi_beacon_client,
        chain_info.clone(),
        validator_preferences.clone(),
        config.clone(),
        current_slot_info,
        merge_requests_tx,
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

async fn init_broadcasters(config: &RelayConfig) -> Vec<Arc<BlockBroadcaster>> {
    let mut broadcasters = vec![];
    for cfg in &config.broadcasters {
        match cfg {
            BroadcasterConfig::BeaconClient(cfg) => {
                broadcasters.push(Arc::new(BlockBroadcaster::BeaconClient(
                    BeaconClient::from_config(cfg.clone()),
                )));
            }
        }
    }
    broadcasters
}
