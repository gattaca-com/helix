use std::{env, net::SocketAddr, sync::Arc, time::Duration};

use ethereum_consensus::crypto::SecretKey;
use moka::sync::Cache;
use tokio::{
    sync::broadcast,
    time::{sleep, timeout},
};
use tracing::{error, info};

use crate::{
    builder::optimistic_simulator::OptimisticSimulator,
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    relay_data::{BidsCache, DeliveredPayloadsCache},
    router::{build_router, BuilderApiProd, DataApiProd, ProposerApiProd},
};
use helix_beacon_client::{
    beacon_client::BeaconClient, fiber_broadcaster::FiberBroadcaster,
    multi_beacon_client::MultiBeaconClient, BlockBroadcaster, MultiBeaconClientTrait,
};
use helix_common::{
    chain_info::ChainInfo, signing::RelaySigningContext, BroadcasterConfig, NetworkConfig,
    RelayConfig,
};
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
use helix_datastore::redis::redis_cache::RedisCache;
use helix_housekeeper::{ChainEventUpdater, Housekeeper};

pub(crate) const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SIMULATOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INIT_BROADCASTER_TIMEOUT: Duration = Duration::from_secs(30);

const HEAD_EVENT_CHANNEL_SIZE: usize = 100;
const PAYLOAD_ATTRIBUTE_CHANNEL_SIZE: usize = 300;

pub struct ApiService {}

impl ApiService {
    pub async fn run(mut config: RelayConfig) {
        let postgres_db = PostgresDatabaseService::from_relay_config(&config).unwrap();
        postgres_db.run_migrations().await;
        postgres_db.init_region(&config).await;
        postgres_db
            .store_builders_info(&config.builders)
            .await
            .expect("failed to store builders info from config");
        postgres_db.load_known_validators().await;
        postgres_db.start_registration_processor().await;

        let db = Arc::new(postgres_db);

        let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");

        let auctioneer = Arc::new(RedisCache::new(&config.redis.url, builder_infos).await.unwrap());

        let auctioneer_clone = auctioneer.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = auctioneer_clone.start_best_bid_listener().await {
                    tracing::error!("Bid listener error: {}", err);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        });

        let broadcasters = init_broadcasters(&config).await;

        let mut beacon_clients = vec![];
        for cfg in &config.beacon_clients {
            beacon_clients.push(Arc::new(BeaconClient::from_config(cfg.clone())));
        }
        let multi_beacon_client = Arc::new(MultiBeaconClient::<BeaconClient>::new(beacon_clients));

        // Subscribe to head and payload attribute events
        let (head_event_sender, head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
        multi_beacon_client.subscribe_to_head_events(head_event_sender).await;
        let (payload_attribute_sender, payload_attribute_receiver) =
            broadcast::channel(PAYLOAD_ATTRIBUTE_CHANNEL_SIZE);
        multi_beacon_client.subscribe_to_payload_attributes_events(payload_attribute_sender).await;

        let chain_info = Arc::new(match config.network_config {
            NetworkConfig::Mainnet => ChainInfo::for_mainnet(),
            NetworkConfig::Goerli => ChainInfo::for_goerli(),
            NetworkConfig::Sepolia => ChainInfo::for_sepolia(),
            NetworkConfig::Holesky => ChainInfo::for_holesky(),
            NetworkConfig::Custom { ref dir_path, ref genesis_validator_root, genesis_time } => {
                match ChainInfo::for_custom(dir_path.clone(), *genesis_validator_root, genesis_time)
                {
                    Ok(chain_info) => chain_info,
                    Err(err) => {
                        error!("Failed to load custom chain info: {:?}", err);
                        std::process::exit(1);
                    }
                }
            }
        });

        let housekeeper = Housekeeper::new(
            db.clone(),
            multi_beacon_client.clone(),
            auctioneer.clone(),
            config.clone(),
            chain_info.clone(),
        );
        let mut housekeeper_head_events = head_event_receiver.resubscribe();
        tokio::spawn(async move {
            loop {
                if let Err(err) = housekeeper.start(&mut housekeeper_head_events).await {
                    tracing::error!("Housekeeper error: {}", err);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        });

        // Initialise relay signing context
        let signing_key_str = env::var("RELAY_KEY").expect("could not find RELAY_KEY in env");
        let signing_key = SecretKey::try_from(signing_key_str)
            .expect("could not convert env signing key to SecretKey");
        let public_key = signing_key.public_key();
        info!(relay_pub_key = ?public_key);
        let relay_signing_context = Arc::new(RelaySigningContext {
            signing_key,
            public_key,
            context: chain_info.context.clone(),
        });

        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let simulator = OptimisticSimulator::<RedisCache, PostgresDatabaseService>::new(
            auctioneer.clone(),
            db.clone(),
            client,
            config.simulator.url.clone(),
        );

        let (mut chain_event_updater, slot_update_sender) =
            ChainEventUpdater::new(db.clone(), chain_info.clone());

        let chain_updater_head_events = head_event_receiver.resubscribe();
        let chain_updater_payload_events = payload_attribute_receiver.resubscribe();
        tokio::spawn(async move {
            chain_event_updater
                .start(chain_updater_head_events, chain_updater_payload_events)
                .await;
        });

        let gossiper = Arc::new(
            GrpcGossiperClientManager::new(
                config.relays.iter().map(|cfg| cfg.url.clone()).collect(),
            )
            .await
            .expect("failed to initialise gRPC gossiper"),
        );

        let validator_preferences = Arc::new(config.validator_preferences.clone());

        let (builder_gossip_sender, builder_gossip_receiver) = tokio::sync::mpsc::channel(10_000);
        let (proposer_gossip_sender, proposer_gossip_receiver) = tokio::sync::mpsc::channel(10_000);

        let builder_api = Arc::new(BuilderApiProd::new(
            auctioneer.clone(),
            db.clone(),
            chain_info.clone(),
            simulator,
            gossiper.clone(),
            relay_signing_context,
            config.clone(),
            slot_update_sender.clone(),
            builder_gossip_receiver,
            validator_preferences.clone(),
        ));

        gossiper.start_server(builder_gossip_sender, proposer_gossip_sender).await;

        let proposer_api = Arc::new(ProposerApiProd::new(
            auctioneer.clone(),
            db.clone(),
            gossiper.clone(),
            broadcasters,
            multi_beacon_client.clone(),
            chain_info.clone(),
            slot_update_sender,
            validator_preferences.clone(),
            config.target_get_payload_propagation_duration_ms,
            proposer_gossip_receiver,
        ));

        let data_api = Arc::new(DataApiProd::new(validator_preferences.clone(), db.clone()));

        let bids_cache: Arc<BidsCache> = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .time_to_idle(Duration::from_secs(5))
                .build(),
        );

        let delivered_payloads_cache: Arc<DeliveredPayloadsCache> = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .time_to_idle(Duration::from_secs(5))
                .build(),
        );

        let router = build_router(
            &mut config.router_config,
            builder_api,
            proposer_api,
            data_api,
            bids_cache,
            delivered_payloads_cache,
        );

        let listener = tokio::net::TcpListener::bind("0.0.0.0:4040").await.unwrap();
        match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .await
        {
            Ok(_) => info!("Server exited successfully"),
            Err(e) => error!("Server exited with error: {e}"),
        }
    }
}

async fn init_broadcasters(config: &RelayConfig) -> Vec<Arc<BlockBroadcaster>> {
    let mut broadcasters = vec![];
    for cfg in &config.broadcasters {
        match cfg {
            BroadcasterConfig::Fiber(cfg) => {
                let result = timeout(
                    INIT_BROADCASTER_TIMEOUT,
                    FiberBroadcaster::new(cfg.url.clone(), cfg.api_key.clone(), cfg.encoding),
                )
                .await;
                match result {
                    Ok(Ok(broadcaster)) => {
                        broadcasters.push(Arc::new(BlockBroadcaster::Fiber(broadcaster)));
                    }
                    Ok(Err(err)) => {
                        error!(broadcaster = "Fiber", cfg = ?cfg, error = %err, "Initializing broadcaster failed");
                    }
                    Err(err) => {
                        error!(broadcaster = "Fiber", cfg = ?cfg, error = %err, "Initializing broadcaster timed out");
                    }
                }
            }
            BroadcasterConfig::BeaconClient(cfg) => {
                broadcasters.push(Arc::new(BlockBroadcaster::BeaconClient(
                    BeaconClient::from_config(cfg.clone()),
                )));
            }
        }
    }
    broadcasters
}

// add test module
#[cfg(test)]
mod test {

    use helix_common::{BeaconClientConfig, FiberConfig};
    use helix_utils::request_encoding::Encoding;

    use super::*;
    use std::convert::TryFrom;
    use url::Url;

    #[test]
    fn test() {
        let signing_key = SecretKey::try_from(
            "0x123456789573772b8ffd9deddb468017a73cae08451ef05e604194705a1bade8".to_string(),
        )
        .expect("could not convert env signing key to SecretKey");
        let public_key = signing_key.public_key();
        assert_eq!(format!("{:?}", public_key), "0x99c8b06e7626f20754156946717a3be789c10bcd1979536dbf71003c58475b489ab3982e85d7ed0b7b5ad1cbc381d65d");
    }

    #[tokio::test]
    async fn test_init_broadcasters_timeout_triggered() {
        let mut config = RelayConfig::default();
        config.broadcasters = vec![
            BroadcasterConfig::Fiber(FiberConfig {
                url: "http://localhost:4040".to_string(),
                api_key: "123".to_string(),
                encoding: Encoding::Json,
            }),
            BroadcasterConfig::BeaconClient(BeaconClientConfig {
                url: Url::parse("http://localhost:4040").unwrap(),
                gossip_blobs_enabled: false,
            }),
        ];
        let broadcasters = init_broadcasters(&config).await;
        assert_eq!(broadcasters.len(), 1);
    }
}
