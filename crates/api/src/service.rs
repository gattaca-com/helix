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
    bid_sorter::{BestGetHeader, BidSorterMessage, FloorBid},
    chain_info::ChainInfo,
    merging_pool::MergingPoolMessage,
    signing::RelaySigningContext,
    BroadcasterConfig, RelayConfig,
};
use helix_datastore::Auctioneer;
use helix_housekeeper::CurrentSlotInfo;
use moka::sync::Cache;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{
    builder::{
        self, api::BuilderApi, multi_simulator::MultiSimulator,
        optimistic_simulator::OptimisticSimulator, v2_check::V2SubChecker,
    },
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
    auctioneer: Arc<A::Auctioneer>,
    current_slot_info: CurrentSlotInfo,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<A::MetadataProvider>,
    terminating: Arc<AtomicBool>,
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    pool_tx: crossbeam_channel::Sender<MergingPoolMessage>,
    pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    shared_best_header: BestGetHeader,
    shared_floor: FloorBid,
) {
    let broadcasters = init_broadcasters(&config).await;

    let client = reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

    let mut simulators = vec![];

    for cfg in &config.simulators {
        let simulator = OptimisticSimulator::<A::Auctioneer, A::DatabaseService>::new(
            auctioneer.clone(),
            db.clone(),
            client.clone(),
            cfg.clone(),
        );
        simulators.push(simulator);
    }

    let simulator = MultiSimulator::new(simulators);
    let sim = simulator.clone();
    tokio::spawn(sim.start_sync_monitor());

    let gossiper = Arc::new(
        GrpcGossiperClientManager::new(config.relays.iter().map(|cfg| cfg.url.clone()).collect())
            .await
            .expect("failed to initialise gRPC gossiper"),
    );

    let validator_preferences = Arc::new(config.validator_preferences.clone());

    let (gossip_sender, gossip_receiver) = tokio::sync::mpsc::channel(10_000);

    let (v2_checks_tx, v2_checks_rx) = tokio::sync::mpsc::channel(10_000);
    let v2_checker = V2SubChecker::<A>::new(v2_checks_rx, auctioneer.clone(), db.clone());
    tokio::spawn(v2_checker.run());

    let builder_api = BuilderApi::<A>::new(
        auctioneer.clone(),
        db.clone(),
        chain_info.clone(),
        simulator.clone(),
        gossiper.clone(),
        metadata_provider.clone(),
        config.clone(),
        validator_preferences.clone(),
        current_slot_info.clone(),
        sorter_tx,
        pool_tx,
        top_bid_tx,
        v2_checks_tx,
        shared_floor,
        shared_best_header.clone(),
    );
    let builder_api = Arc::new(builder_api);

    gossiper.start_server(gossip_sender).await;

    let (v3_payload_request_send, v3_payload_request_recv) = mpsc::channel(32);
    if let Some(v3_port) = config.v3_port {
        // v3 tcp optimistic configured
        tokio::spawn(builder::v3::tcp::run_api(v3_port, builder_api.clone()));
    }

    // Start builder block fetcher
    tokio::spawn(builder::v3::payload::fetch_builder_blocks(
        builder_api.clone(),
        v3_payload_request_recv,
        relay_signing_context.clone(),
    ));

    if config.inclusion_list.is_some() {
        tokio::spawn(listen_for_inclusion_lists_background_task(builder_api.clone()));
    }

    let proposer_api = Arc::new(ProposerApi::<A>::new(
        auctioneer.clone(),
        db.clone(),
        simulator,
        gossiper.clone(),
        metadata_provider.clone(),
        relay_signing_context,
        broadcasters,
        multi_beacon_client,
        chain_info.clone(),
        validator_preferences.clone(),
        config.clone(),
        v3_payload_request_send,
        current_slot_info,
        shared_best_header,
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

async fn listen_for_inclusion_lists_background_task(api: Arc<BuilderApi<impl Api>>) -> ! {
    info!("Starting to listen for inclusion list updates");

    let mut inclusion_list_recv = api.auctioneer.get_inclusion_list();
    loop {
        if let Ok(new_list) = inclusion_list_recv.recv().await {
            api.current_inclusion_list.write().replace(new_list);
        }
    }
}

#[cfg(test)]
mod test {
    use alloy_primitives::hex;
    use helix_common::BeaconClientConfig;
    use helix_types::BlsSecretKey;
    use url::Url;

    use super::*;

    #[test]
    fn test() {
        let signing_key = BlsSecretKey::deserialize(
            hex!("123456789573772b8ffd9deddb468017a73cae08451ef05e604194705a1bade8").as_slice(),
        )
        .expect("could not convert env signing key to SecretKey");
        let public_key = signing_key.public_key();
        assert_eq!(format!("{:?}", public_key), "0x99c8b06e7626f20754156946717a3be789c10bcd1979536dbf71003c58475b489ab3982e85d7ed0b7b5ad1cbc381d65d");
    }

    #[tokio::test]
    async fn test_init_broadcasters_timeout_triggered() {
        let config = RelayConfig {
            broadcasters: vec![BroadcasterConfig::BeaconClient(BeaconClientConfig {
                url: Url::parse("http://localhost:4040").unwrap(),
                gossip_blobs_enabled: false,
            })],
            ..Default::default()
        };
        let broadcasters = init_broadcasters(&config).await;
        assert_eq!(broadcasters.len(), 1);
    }
}
