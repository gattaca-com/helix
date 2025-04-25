use std::{net::SocketAddr, sync::Arc, time::Duration};

use helix_beacon::{
    beacon_client::BeaconClient, fiber_broadcaster::FiberBroadcaster,
    multi_beacon_client::MultiBeaconClient, BlockBroadcaster,
};
use helix_common::{
    chain_info::ChainInfo, signing::RelaySigningContext, BroadcasterConfig, RelayConfig,
};
use helix_housekeeper::CurrentSlotInfo;
use moka::sync::Cache;
use tokio::{sync::mpsc, time::timeout};
use tracing::{error, info};

use crate::{
    builder::{
        self, api::BuilderApi, multi_simulator::MultiSimulator,
        optimistic_simulator::OptimisticSimulator,
    },
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::ProposerApi,
    relay_data::{BidsCache, DataApi, DeliveredPayloadsCache},
    router::build_router,
    Api,
};

pub(crate) const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SIMULATOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INIT_BROADCASTER_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ApiService;

impl ApiService {
    pub async fn run<A: Api>(
        mut config: RelayConfig,
        db: Arc<A::DatabaseService>,
        auctioneer: Arc<A::Auctioneer>,
        current_slot_info: CurrentSlotInfo,
        chain_info: Arc<ChainInfo>,
        relay_signing_context: Arc<RelaySigningContext>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        metadata_provider: Arc<A::MetadataProvider>,
    ) {
        let broadcasters = init_broadcasters(&config).await;

        let client =
            reqwest::ClientBuilder::new().timeout(SIMULATOR_REQUEST_TIMEOUT).build().unwrap();

        let mut simulators = vec![];

        for cfg in &config.simulators {
            let simulator = OptimisticSimulator::<A::Auctioneer, A::DatabaseService>::new(
                auctioneer.clone(),
                db.clone(),
                client.clone(),
                cfg.url.clone(),
            );
            simulators.push(simulator);
        }

        let simulator = MultiSimulator::new(simulators);

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

        let builder_api = BuilderApi::<A>::new(
            auctioneer.clone(),
            db.clone(),
            chain_info.clone(),
            simulator,
            gossiper.clone(),
            metadata_provider.clone(),
            relay_signing_context.clone(),
            config.clone(),
            builder_gossip_receiver,
            validator_preferences.clone(),
            current_slot_info.clone(),
        );
        let builder_api = Arc::new(builder_api);

        gossiper.start_server(builder_gossip_sender, proposer_gossip_sender).await;

        let (v3_payload_request_send, v3_payload_request_recv) = mpsc::channel(32);
        if let Some(v3_port) = config.v3_port {
            // v3 tcp optimistic configured
            tokio::spawn(builder::v3::tcp::run_api(v3_port, builder_api.clone()));
        }

        // Start builder block fetcher
        tokio::spawn(builder::v3::payload::fetch_builder_blocks(
            builder_api.clone(),
            v3_payload_request_recv,
            relay_signing_context,
        ));

        let proposer_api = Arc::new(ProposerApi::<A>::new(
            auctioneer.clone(),
            db.clone(),
            gossiper.clone(),
            metadata_provider.clone(),
            broadcasters,
            multi_beacon_client,
            chain_info.clone(),
            validator_preferences.clone(),
            proposer_gossip_receiver,
            config.clone(),
            v3_payload_request_send,
            current_slot_info,
        ));

        let data_api = Arc::new(DataApi::<A>::new(validator_preferences.clone(), db.clone()));

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
