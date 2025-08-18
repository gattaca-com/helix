use std::{sync::Arc, time::Duration};

use axum::{
    error_handling::HandleErrorLayer,
    http::StatusCode,
    routing::{get, post},
    BoxError, Extension, Router,
};
use helix_beacon::{
    beacon_client::mock_beacon_node::MockBeaconNode, multi_beacon_client::MultiBeaconClient,
    BlockBroadcaster,
};
use helix_common::{
    api::{
        PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_API, PATH_GET_HEADER, PATH_GET_PAYLOAD,
        PATH_PROPOSER_API, PATH_PROPOSER_PAYLOAD_DELIVERED, PATH_REGISTER_VALIDATORS, PATH_STATUS,
        PATH_VALIDATOR_REGISTRATION,
    },
    bid_sorter::{BestGetHeader, FloorBid},
    chain_info::ChainInfo,
    merging_pool::BestMergeableOrders,
    metadata_provider::DefaultMetadataProvider,
    signing::RelaySigningContext,
    RelayConfig, Route, ValidatorPreferences,
};
use helix_database::mock_database_service::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_housekeeper::CurrentSlotInfo;
use tokio::sync::{
    broadcast,
    mpsc::{self, channel},
};
use tower::{buffer::BufferLayer, limit::RateLimitLayer, timeout::TimeoutLayer, ServiceBuilder};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{
    builder::{
        api::{BuilderApi, MAX_PAYLOAD_LENGTH},
        multi_simulator::MultiSimulator,
    },
    constants::{MAX_BLINDED_BLOCK_LENGTH, _MAX_VAL_REGISTRATIONS_LENGTH},
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::{self, ProposerApi},
    relay_data::DataApi,
    Api,
};

#[derive(Clone)]
pub struct MockApi;

impl Api for MockApi {
    type Auctioneer = MockAuctioneer;
    type DatabaseService = MockDatabaseService;
    type MetadataProvider = DefaultMetadataProvider;
}

pub fn app() -> Router {
    let (v3_sender, _v3_receiver) = channel(32);

    let node = MockBeaconNode::new();
    let client = node.beacon_client();

    let api_service = Arc::new(ProposerApi::<MockApi>::new(
        Arc::new(MockAuctioneer::default()),
        Arc::new(MockDatabaseService::default()),
        MultiSimulator::new(vec![]),
        GrpcGossiperClientManager::mock().into(),
        Arc::new(DefaultMetadataProvider::default()),
        Arc::new(RelaySigningContext::default()),
        vec![Arc::new(BlockBroadcaster::BeaconClient(client))],
        Arc::new(MultiBeaconClient::new(vec![])),
        Arc::new(ChainInfo::for_mainnet()),
        Arc::new(ValidatorPreferences::default()),
        Default::default(),
        v3_sender,
        Default::default(),
        BestGetHeader::new(),
        BestMergeableOrders::new(),
    ));

    let data_api = Arc::new(DataApi::<MockApi>::new(
        Arc::new(ValidatorPreferences::default()),
        Arc::new(MockDatabaseService::default()),
    ));

    Router::new()
        .route(&format!("{PATH_PROPOSER_API}{PATH_STATUS}"), get(proposer::status))
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
            post(ProposerApi::<MockApi>::register_validators),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
            get(ProposerApi::<MockApi>::get_header),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
            post(ProposerApi::<MockApi>::get_payload),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
            get(DataApi::<MockApi>::proposer_payload_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            get(DataApi::<MockApi>::builder_bids_received),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            get(DataApi::<MockApi>::validator_registration),
        )
        .layer(Extension(api_service))
        .layer(Extension(data_api))
}

pub fn builder_api_app() -> (Router, Arc<BuilderApi<MockApi>>, CurrentSlotInfo) {
    let current_slot_info = CurrentSlotInfo::new();
    let (sort_tx, _) = crossbeam_channel::bounded(1000);
    let (br_tx, _) = broadcast::channel(1);
    let (v2_tx, _) = mpsc::channel(100);
    let shared_floor = FloorBid::new();

    let builder_api_service = BuilderApi::<MockApi>::new(
        Arc::new(MockAuctioneer::default()),
        Arc::new(MockDatabaseService::default()),
        Arc::new(ChainInfo::for_mainnet()),
        MultiSimulator::new(vec![]),
        GrpcGossiperClientManager::mock().into(),
        Arc::new(DefaultMetadataProvider::default()),
        RelayConfig::default(),
        Arc::new(ValidatorPreferences::default()),
        current_slot_info.clone(),
        sort_tx,
        br_tx,
        v2_tx,
        shared_floor,
        BestGetHeader::new(),
    );
    let builder_api_service = Arc::new(builder_api_service);

    let mut router = Router::new()
        .route(&Route::GetValidators.path(), get(BuilderApi::<MockApi>::get_validators))
        .route(&Route::SubmitBlock.path(), post(BuilderApi::<MockApi>::submit_block))
        .route(&Route::GetTopBid.path(), get(BuilderApi::<MockApi>::get_top_bid))
        .layer(RequestBodyLimitLayer::new(MAX_PAYLOAD_LENGTH))
        .layer(Extension(builder_api_service.clone()));

    // Add Timeout-Layer
    // Add Rate-Limit-Layer (buffered so we can clone the service)
    // Add Error-handling layer
    router = router.layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(|_: BoxError| async { StatusCode::REQUEST_TIMEOUT }))
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .layer(HandleErrorLayer::new(|err: BoxError| async move {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Unhandled error: {}", err))
            }))
            .layer(BufferLayer::new(4096))
            .layer(RateLimitLayer::new(100, Duration::from_secs(1))),
    );

    (router, builder_api_service, current_slot_info)
}

pub fn proposer_api_app(
) -> (Router, Arc<ProposerApi<MockApi>>, CurrentSlotInfo, Arc<MockAuctioneer>) {
    let (v3_sender, _v3_receiver) = channel(32);
    let auctioneer = Arc::new(MockAuctioneer::default());
    let node = MockBeaconNode::new();
    let client = node.beacon_client();

    let current_slot_info = CurrentSlotInfo::new();
    let proposer_api_service = Arc::new(ProposerApi::<MockApi>::new(
        auctioneer.clone(),
        Arc::new(MockDatabaseService::default()),
        MultiSimulator::new(vec![]),
        GrpcGossiperClientManager::mock().into(),
        Arc::new(DefaultMetadataProvider::default()),
        Arc::new(RelaySigningContext::default()),
        vec![Arc::new(BlockBroadcaster::BeaconClient(client))],
        Arc::new(MultiBeaconClient::new(vec![])),
        Arc::new(ChainInfo::for_mainnet()),
        Arc::new(ValidatorPreferences::default()),
        Default::default(),
        v3_sender,
        current_slot_info.clone(),
        BestGetHeader::new(),
        BestMergeableOrders::new(),
    ));

    let router = Router::new()
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
            get(ProposerApi::<MockApi>::get_header),
        )
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
            post(ProposerApi::<MockApi>::get_payload),
        )
        .layer(RequestBodyLimitLayer::new(MAX_BLINDED_BLOCK_LENGTH))
        .route(
            &format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
            post(ProposerApi::<MockApi>::register_validators),
        )
        .layer(RequestBodyLimitLayer::new(_MAX_VAL_REGISTRATIONS_LENGTH))
        .layer(Extension(proposer_api_service.clone()));

    (router, proposer_api_service, current_slot_info, auctioneer)
}

pub fn data_api_app() -> (Router, Arc<DataApi<MockApi>>, Arc<MockDatabaseService>) {
    let mock_database = Arc::new(MockDatabaseService::default());
    let proposer_api_service = Arc::new(DataApi::<MockApi>::new(
        Arc::new(ValidatorPreferences::default()),
        mock_database.clone(),
    ));

    let router = Router::new()
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
            get(DataApi::<MockApi>::proposer_payload_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            get(DataApi::<MockApi>::builder_bids_received),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            get(DataApi::<MockApi>::validator_registration),
        )
        .layer(Extension(proposer_api_service.clone()));

    (router, proposer_api_service, mock_database)
}
