use axum::{
    error_handling::HandleErrorLayer,
    http::StatusCode,
    routing::{get, post},
    Extension, Router,
};
use helix_beacon_client::{beacon_client::BeaconClient, multi_beacon_client::MultiBeaconClient};
use helix_common::{Route, RouterConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
use std::sync::Arc;
use tower::{timeout::TimeoutLayer, BoxError, ServiceBuilder};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{
    builder::{
        api::{BuilderApi, MAX_HEADER_LENGTH, MAX_PAYLOAD_LENGTH},
        optimistic_simulator::OptimisticSimulator,
        PATH_BUILDER_API, PATH_GET_VALIDATORS, PATH_SUBMIT_BLOCK, PATH_SUBMIT_BLOCK_OPTIMISTIC_V2,
        PATH_SUBMIT_HEADER,
    },
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::{
        api::{ProposerApi, MAX_BLINDED_BLOCK_LENGTH, MAX_VAL_REGISTRATIONS_LENGTH},
        PATH_GET_HEADER, PATH_GET_PAYLOAD, PATH_PROPOSER_API, PATH_REGISTER_VALIDATORS,
        PATH_STATUS,
    },
    relay_data::{
        DataApi, PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_API, PATH_PROPOSER_PAYLOAD_DELIVERED,
        PATH_VALIDATOR_REGISTRATION,
    },
    service::API_REQUEST_TIMEOUT,
};

pub type BuilderApiProd = BuilderApi<
    RedisCache,
    PostgresDatabaseService,
    OptimisticSimulator<RedisCache, PostgresDatabaseService>,
    GrpcGossiperClientManager,
>;

pub type ProposerApiProd =
    ProposerApi<RedisCache, PostgresDatabaseService, MultiBeaconClient<BeaconClient>>;

pub type DataApiProd = DataApi<PostgresDatabaseService>;

pub fn build_router(
    router_config: &mut RouterConfig,
    builder_api: Arc<BuilderApiProd>,
    proposer_api: Arc<ProposerApiProd>,
    data_api: Arc<DataApiProd>,
) -> Router {
    router_config.resolve_condensed_routes();

    let mut router = Router::new();

    for &route in &router_config.enabled_routes {
        match route {
            Route::GetValidators => {
                router = router.route(
                    &format!("{PATH_BUILDER_API}{PATH_GET_VALIDATORS}"),
                    get(BuilderApiProd::get_validators),
                );
            }
            Route::SubmitBlock => {
                router = router
                    .route(
                        &format!("{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK}"),
                        post(BuilderApiProd::submit_block),
                    )
                    .layer(RequestBodyLimitLayer::new(MAX_PAYLOAD_LENGTH));
            }
            Route::SubmitBlockOptimistic => {
                router = router
                    .route(
                        &format!("{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK_OPTIMISTIC_V2}"),
                        post(BuilderApiProd::submit_block_v2),
                    )
                    .layer(RequestBodyLimitLayer::new(MAX_PAYLOAD_LENGTH));
            }
            Route::SubmitHeader => {
                router = router
                    .route(
                        &format!("{PATH_BUILDER_API}{PATH_SUBMIT_HEADER}"),
                        post(BuilderApiProd::submit_header),
                    )
                    .layer(RequestBodyLimitLayer::new(MAX_HEADER_LENGTH));
            }
            Route::Status => {
                router = router.route(
                    &format!("{PATH_PROPOSER_API}{PATH_STATUS}"),
                    get(ProposerApiProd::status),
                );
            }
            Route::RegisterValidators => {
                router = router
                    .route(
                        &format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
                        post(ProposerApiProd::register_validators),
                    )
                    .layer(RequestBodyLimitLayer::new(MAX_VAL_REGISTRATIONS_LENGTH));
            }
            Route::GetHeader => {
                router = router.route(
                    &format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
                    get(ProposerApiProd::get_header),
                );
            }
            Route::GetPayload => {
                router = router
                    .route(
                        &format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
                        post(ProposerApiProd::get_payload),
                    )
                    .layer(RequestBodyLimitLayer::new(MAX_BLINDED_BLOCK_LENGTH));
            }
            Route::ProposerPayloadDelivered => {
                router = router.route(
                    &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
                    get(DataApiProd::proposer_payload_delivered),
                );
            }
            Route::BuilderBidsReceived => {
                router = router.route(
                    &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
                    get(DataApiProd::builder_bids_received),
                );
            }
            Route::ValidatorRegistration => {
                router = router.route(
                    &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
                    get(DataApiProd::validator_registration),
                );
            }
            _ => {
                panic!("Route not implemented: {:?}, please add handling if there are new routes or resolve condensed routes before!", route);
            }
        }
    }

    // Add Timeout-Layer
    // Add Error-handling layer
    router = router.layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(|_: BoxError| async { StatusCode::REQUEST_TIMEOUT }))
            .layer(TimeoutLayer::new(API_REQUEST_TIMEOUT)),
    );

    // Add Extension layers
    router = router
        .layer(Extension(builder_api))
        .layer(Extension(proposer_api))
        .layer(Extension(data_api));

    router
}
