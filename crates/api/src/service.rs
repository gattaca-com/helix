use std::{env, sync::Arc, time::Duration};

use ethereum_consensus::crypto::SecretKey;
use tokio::time::sleep;
use tracing::info;

use crate::builder::optimistic_simulator::OptimisticSimulator;
use crate::gossiper::grpc_gossiper::GrpcGossiperClientManager;
use crate::router::{build_router, BuilderApiProd, DataApiProd, ProposerApiProd};
use helix_beacon_client::{
    beacon_client::BeaconClient,
    fiber_broadcaster::FiberBroadcaster, multi_beacon_client::MultiBeaconClient, BlockBroadcaster,
};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_database::DatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
use helix_housekeeper::{ChainEventUpdater, Housekeeper};
use helix_common::{
    fork_info::ForkInfo, signing::RelaySigningContext,
    BroadcasterConfig, ForkInfoConfig, RelayConfig,
};

pub struct ApiService {}

impl ApiService {
    pub async fn run(mut config: RelayConfig) {
        let postgres_db = PostgresDatabaseService::from_relay_config(&config).unwrap();
        postgres_db.run_migrations().await;
        postgres_db.init_region(&config).await;
        postgres_db.start_registration_processor().await;

        let db = Arc::new(postgres_db);

        let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");
        let auctioneer = Arc::new(RedisCache::new(&config.redis.url, builder_infos).await.unwrap());
        let broadcasters = init_broadcasters(&config).await;

        let mut beacon_clients = vec![];
        for cfg in config.beacon_clients {
            beacon_clients.push(Arc::new(BeaconClient::from_endpoint_str(&cfg.url)));
        }
        let multi_beacon_client = Arc::new(MultiBeaconClient::<BeaconClient>::new(beacon_clients));

        let fork_info = Arc::new(match config.fork_info {
            ForkInfoConfig::Mainnet => ForkInfo::for_mainnet(),
            ForkInfoConfig::Goerli => ForkInfo::for_goerli(),
            ForkInfoConfig::Sepolia => ForkInfo::for_sepolia(),
            ForkInfoConfig::Holesky => ForkInfo::for_holesky(),
        });

        // Housekeeper should only be run on one instance.
        if config.run_housekeeper {
            let housekeeper =
                Housekeeper::new(db.clone(), multi_beacon_client.clone(), auctioneer.clone(), fork_info.clone());
            tokio::task::spawn(async move {
                loop {
                    if let Err(err) = housekeeper.start().await {
                        tracing::error!("Housekeeper error: {}", err);
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            });
        }

        // Initialise relay signing context
        let signing_key_str = env::var("RELAY_KEY").expect("could not find RELAY_KEY in env");
        let signing_key = SecretKey::try_from(signing_key_str)
            .expect("could not convert env signing key to SecretKey");
        let public_key = signing_key.public_key();
        info!(relay_pub_key = ?public_key);
        let relay_signing_context = Arc::new(RelaySigningContext {
            signing_key,
            public_key,
            context: fork_info.context.clone(),
        });

        let simulator = OptimisticSimulator::<RedisCache, PostgresDatabaseService>::new(
            auctioneer.clone(),
            db.clone(),
            reqwest::Client::new(),
            config.simulator.url,
        );

        let (mut chain_event_updater, slot_update_sender) =
            ChainEventUpdater::new_with_channel(db.clone());

        let mbc_clone = multi_beacon_client.clone();
        tokio::spawn(async move {
            chain_event_updater.start(mbc_clone).await;
        });

        let gossiper = Arc::new(
            GrpcGossiperClientManager::new(
                config.relays.iter().map(|cfg| cfg.url.clone()).collect(),
            )
            .await
            .expect("failed to initialise gRPC gossiper"),
        );

        let (gossip_sender, gossip_receiver) = tokio::sync::mpsc::channel(10_000);

        let builder_api = Arc::new(BuilderApiProd::new(
            auctioneer.clone(),
            db.clone(),
            fork_info.clone(),
            simulator,
            gossiper.clone(),
            relay_signing_context,
            slot_update_sender.clone(),
            gossip_receiver,
        ));

        gossiper.start_server(gossip_sender).await;

        let proposer_api = Arc::new(ProposerApiProd::new(
            auctioneer.clone(),
            db.clone(),
            broadcasters,
            multi_beacon_client.clone(),
            fork_info.clone(),
            slot_update_sender,
            Arc::new(config.validator_preferences.clone()),
            config.target_get_payload_propagation_duration_ms,
        ));

        let data_api = Arc::new(DataApiProd::new(db.clone()));

        let router = build_router(&mut config.router_config, builder_api, proposer_api, data_api);

        match axum::Server::bind(&"0.0.0.0:4040".parse().unwrap())
            .serve(router.into_make_service())
            .await
        {
            Ok(_) => println!("Server exited successfully"),
            Err(e) => println!("Server exited with error: {e}"),
        }
    }
}

async fn init_broadcasters(config: &RelayConfig) -> Vec<Arc<BlockBroadcaster>> {
    let mut broadcasters = vec![];
    for cfg in &config.broadcasters {
        match cfg {
            BroadcasterConfig::Fiber(cfg) => {
                broadcasters.push(Arc::new(BlockBroadcaster::Fiber(
                    FiberBroadcaster::new(cfg.url.clone(), cfg.api_key.clone(), cfg.encoding).await,
                )));
            }
            BroadcasterConfig::BeaconClient(cfg) => {
                broadcasters.push(Arc::new(BlockBroadcaster::BeaconClient(
                    BeaconClient::from_endpoint_str(&cfg.url),
                )));
            }
        }
    }
    broadcasters
}

// add test module
#[cfg(test)]
mod test {

    use super::*;
    use std::convert::TryFrom;

    #[test]
    fn test() {
        let signing_key = SecretKey::try_from(
            "0x123456789573772b8ffd9deddb468017a73cae08451ef05e604194705a1bade8".to_string(),
        )
        .expect("could not convert env signing key to SecretKey");
        let public_key = signing_key.public_key();
        assert_eq!(format!("{:?}", public_key), "0x99c8b06e7626f20754156946717a3be789c10bcd1979536dbf71003c58475b489ab3982e85d7ed0b7b5ad1cbc381d65d");
    }
}
