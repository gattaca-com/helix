use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use axum::serve::ListenerExt;
use crossbeam_channel::Sender;
use flux::spine::StandaloneDCacheProducer;
use flux_utils::SharedVector;
use helix_common::{
    CurrentSlotInfo, RelayConfig, alerts::AlertManager, api::builder_api::TopBidPrecision,
    local_cache::LocalCache,
};
use helix_data_api::{
    BidsCache, BidsCacheV2, DataApi, DeliveredPayloadsCache, DeliveredPayloadsCacheV2,
    SelectiveExpiry,
};
use helix_operator::OperatorPubSub;
use moka::sync::Cache;
use tokio::sync::mpsc::{Receiver, Sender as MpscSender};
use tracing::{error, info};

use crate::{
    AuctioneerHandle, DbHandle, PostgresDatabaseService,
    api::{
        Api, FutureBidSubmissionResult, blacklist::BlacklistCache, builder::api::BuilderApi,
        extract::raw_web_socket::RawWebSocket, proposer::ProposerApi, router::build_router,
    },
    gossip::{GossipedMessage, process_gossip_messages},
    network::api::RelayNetworkApi,
    spine::messages::NewBidSubmission,
};

pub(crate) const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn start_api_service<A: Api>(
    config: RelayConfig,
    db: Arc<PostgresDatabaseService>,
    local_cache: Arc<LocalCache>,
    current_slot_info: CurrentSlotInfo,
    proposer_api: Arc<ProposerApi<A>>,
    api_provider: Arc<A::ApiProvider>,
    known_validators_loaded: Arc<AtomicBool>,
    terminating: Arc<AtomicBool>,
    relay_network_api: RelayNetworkApi,
    db_handle: DbHandle,
    auctioneer_handle: AuctioneerHandle,
    bid_producer: StandaloneDCacheProducer<NewBidSubmission>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    web_socket_connections: Sender<(RawWebSocket, TopBidPrecision)>,
    alert_manager: Arc<AlertManager>,
    operator_api: Option<Arc<OperatorPubSub>>,
    gossip_sender: MpscSender<GossipedMessage>,
    gossip_receiver: Receiver<GossipedMessage>,
) {
    tokio::spawn(run_api_service::<A>(
        config.clone(),
        db.clone(),
        local_cache.clone(),
        current_slot_info,
        proposer_api,
        api_provider,
        known_validators_loaded,
        terminating.clone(),
        relay_network_api,
        db_handle.clone(),
        auctioneer_handle,
        bid_producer,
        future_results,
        web_socket_connections,
        alert_manager,
        operator_api,
        gossip_sender,
        gossip_receiver,
    ));
}

pub async fn run_api_service<A: Api>(
    mut config: RelayConfig,
    db: Arc<PostgresDatabaseService>,
    local_cache: Arc<LocalCache>,
    current_slot_info: CurrentSlotInfo,
    proposer_api: Arc<ProposerApi<A>>,
    api_provider: Arc<A::ApiProvider>,
    known_validators_loaded: Arc<AtomicBool>,
    terminating: Arc<AtomicBool>,
    relay_network_api: RelayNetworkApi,
    db_handle: DbHandle,
    auctioneer_handle: AuctioneerHandle,
    bid_producer: StandaloneDCacheProducer<NewBidSubmission>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    web_socket_connections: Sender<(RawWebSocket, TopBidPrecision)>,
    alert_manager: Arc<AlertManager>,
    operator_api: Option<Arc<OperatorPubSub>>,
    gossip_sender: MpscSender<GossipedMessage>,
    gossip_receiver: Receiver<GossipedMessage>,
) {
    let validator_preferences = proposer_api.validator_preferences.clone();

    let builder_api = BuilderApi::<A>::new(
        local_cache.clone(),
        db_handle.clone(),
        config.clone(),
        current_slot_info.clone(),
        auctioneer_handle.clone(),
        api_provider.clone(),
        alert_manager.clone(),
        bid_producer,
        future_results,
        web_socket_connections,
        operator_api.clone(),
    );
    let builder_api = Arc::new(builder_api);

    proposer_api.gossiper.start_server(gossip_sender).await;

    tokio::spawn(process_gossip_messages(
        builder_api.clone(),
        proposer_api.clone(),
        gossip_receiver,
    ));

    let data_api =
        Arc::new(DataApi::new(api_provider.clone(), validator_preferences.clone(), db.clone()));

    let bids_cache: BidsCache =
        Cache::builder().time_to_idle(Duration::from_secs(12)).max_capacity(10_000).build();

    let bids_cache_v2: BidsCacheV2 =
        Cache::builder().time_to_idle(Duration::from_secs(12)).max_capacity(10_000).build();

    let delivered_payloads_cache: DeliveredPayloadsCache = Cache::builder()
        .expire_after(SelectiveExpiry)
        .time_to_idle(Duration::from_secs(12))
        .max_capacity(10_000)
        .build();

    let delivered_payloads_cache_v2: DeliveredPayloadsCacheV2 =
        Cache::builder().time_to_idle(Duration::from_secs(12)).max_capacity(10_000).build();

    let blacklist_cache = Arc::new(BlacklistCache::new(config.blacklist_provider.clone()));

    let router = build_router(
        &mut config.router_config,
        builder_api,
        proposer_api,
        data_api,
        relay_network_api,
        bids_cache,
        bids_cache_v2,
        delivered_payloads_cache,
        delivered_payloads_cache_v2,
        blacklist_cache,
        known_validators_loaded,
        terminating,
    );

    let mut servers = tokio::task::JoinSet::new();
    for addr in config.bind_addrs(config.api_port) {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| panic!("failed to bind the API on {addr}: {e}"))
            .tap_io(|tcp_stream| {
                if let Err(e) = tcp_stream.set_nodelay(true) {
                    error!("failed to set TCP_NODELAY on incoming connection: {e}");
                }
            });
        let service = router.clone().into_make_service_with_connect_info::<SocketAddr>();
        servers.spawn(async move { (addr, axum::serve(listener, service).await) });
    }
    while let Some(res) = servers.join_next().await {
        match res {
            Ok((addr, Ok(()))) => info!(%addr, "Server exited successfully"),
            Ok((addr, Err(e))) => error!(%addr, "Server exited with error: {e}"),
            Err(e) => error!("Server task failed: {e}"),
        }
    }
}
