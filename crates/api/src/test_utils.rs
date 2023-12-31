use std::sync::Arc;

use axum::{
    routing::{get, post},
    Extension, Router,
};
use helix_common::{SignedBuilderBid, fork_info::ForkInfo};
use tokio::sync::mpsc::{channel, Receiver, Sender};

use helix_beacon_client::{BlockBroadcaster, mock_block_broadcaster::MockBlockBroadcaster, mock_multi_beacon_client::MockMultiBeaconClient};
use helix_database::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_common::signing::RelaySigningContext;
use helix_housekeeper::ChainUpdate;
use helix_common::api::proposer_api::ValidatorPreferences;

use crate::{
    builder::{
        api::BuilderApi, mock_simulator::MockSimulator, PATH_BUILDER_API, PATH_GET_VALIDATORS,
        PATH_SUBMIT_BLOCK,
    },
    proposer::{
        api::ProposerApi, PATH_GET_HEADER, PATH_GET_PAYLOAD, PATH_PROPOSER_API,
        PATH_REGISTER_VALIDATORS, PATH_STATUS,
    },
    relay_data::{
        DataApi, PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_API, PATH_PROPOSER_PAYLOAD_DELIVERED,
        PATH_VALIDATOR_REGISTRATION,
    },
};
use crate::gossiper::mock_gossiper::MockGossiper;

pub fn app() -> Router {
    let (slot_update_sender, _slot_update_receiver) = channel::<Sender<ChainUpdate>>(32);

    let api_service = Arc::new(ProposerApi::<
        MockAuctioneer,
        MockDatabaseService,
        MockMultiBeaconClient,
    >::new(
        Arc::new(MockAuctioneer::default()),
        Arc::new(MockDatabaseService::default()),
        vec![Arc::new(BlockBroadcaster::Mock(MockBlockBroadcaster::default()))],
        Arc::new(MockMultiBeaconClient::default()),
        Arc::new(ForkInfo::for_mainnet()),
        slot_update_sender,
        Arc::new(ValidatorPreferences::default()),
    ));

    let data_api =
        Arc::new(DataApi::<MockDatabaseService>::new(Arc::new(MockDatabaseService::default())));

    Router::new()
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_STATUS}"),
            get(ProposerApi::<
                MockAuctioneer,
                MockDatabaseService,
                MockMultiBeaconClient,
            >::status),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
            post(
                ProposerApi::<
                    MockAuctioneer,
                    MockDatabaseService,
                    MockMultiBeaconClient,
                >::register_validators,
            ),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
            get(ProposerApi::<
                MockAuctioneer,
                MockDatabaseService,
                MockMultiBeaconClient,
            >::get_header),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
            post(
                ProposerApi::<
                    MockAuctioneer,
                    MockDatabaseService,
                    MockMultiBeaconClient,
                >::get_payload,
            ),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
            get(DataApi::<MockDatabaseService>::proposer_payload_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            get(DataApi::<MockDatabaseService>::builder_bids_received),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            get(DataApi::<MockDatabaseService>::validator_registration),
        )
        .layer(Extension(api_service))
        .layer(Extension(data_api))
}

pub fn builder_api_app() -> (
    Router,
    Arc<BuilderApi<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>>,
    Receiver<Sender<ChainUpdate>>,
) {
    let (slot_update_sender, slot_update_receiver) = channel::<Sender<ChainUpdate>>(32);

    let builder_api_service =
        Arc::new(BuilderApi::<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>::new(
            Arc::new(MockAuctioneer::default()),
            Arc::new(MockDatabaseService::default()),
            Arc::new(ForkInfo::for_mainnet()),
            MockSimulator::default(),
            Arc::new(MockGossiper::new().unwrap()),
            Arc::new(RelaySigningContext::default()),
            slot_update_sender.clone(),
        ));

    let router = Router::new()
        .route(
            &format!("{PATH_BUILDER_API}{PATH_GET_VALIDATORS}"),
            get(BuilderApi::<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>::get_validators),
        )
        .route(
            &format!("{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK}"),
            post(BuilderApi::<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>::submit_block),
        )
        .layer(Extension(builder_api_service.clone()));

    (router, builder_api_service, slot_update_receiver)
}

pub fn proposer_api_app() -> (
    Router, 
    Arc<ProposerApi<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient>>,
    Receiver<Sender<ChainUpdate>>,
    Arc<MockAuctioneer>,
) {
    let (slot_update_sender, slot_update_receiver) = channel::<Sender<ChainUpdate>>(32);
    let auctioneer = Arc::new(MockAuctioneer::default());
    let proposer_api_service =
        Arc::new(ProposerApi::<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient>::new(
            auctioneer.clone(),
            Arc::new(MockDatabaseService::default()),
            vec![Arc::new(BlockBroadcaster::Mock(MockBlockBroadcaster::default()))],
            Arc::new(MockMultiBeaconClient::default()),
            Arc::new(ForkInfo::for_mainnet()),
            slot_update_sender.clone(),
            Arc::new(ValidatorPreferences::default())
        )
    );

    let router = Router::new()
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
            get(ProposerApi::<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient>::get_header),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
            post(ProposerApi::<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient>::get_payload),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
            post(ProposerApi::<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient>::register_validators),
        )
        .layer(Extension(proposer_api_service.clone()));

    (router, proposer_api_service, slot_update_receiver, auctioneer)
}

pub fn data_api_app() -> (
    Router, 
    Arc<DataApi<MockDatabaseService>>,
    Arc<MockDatabaseService>,
) {
    let mock_database = Arc::new(MockDatabaseService::default());
    let proposer_api_service =
        Arc::new(DataApi::<MockDatabaseService>::new(
            mock_database.clone(),
        )
    );

    let router = Router::new()
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
            get(DataApi::<MockDatabaseService>::proposer_payload_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            get(DataApi::<MockDatabaseService>::builder_bids_received),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            get(DataApi::<MockDatabaseService>::validator_registration),
        )
        .layer(Extension(proposer_api_service.clone()));

    (router, proposer_api_service, mock_database)
}