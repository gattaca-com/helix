use axum::{
    error_handling::HandleErrorLayer,
    http::StatusCode,
    middleware,
    routing::{get, post},
    Extension, Router,
};
use helix_beacon_client::{beacon_client::BeaconClient, multi_beacon_client::MultiBeaconClient};
use helix_common::{Route, RouterConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tower::{timeout::TimeoutLayer, BoxError, ServiceBuilder};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{
    builder::{
        api::{BuilderApi, MAX_PAYLOAD_LENGTH},
        optimistic_simulator::OptimisticSimulator,
    },
    constraints::api::ConstraintsApi,
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    middleware::rate_limiting::rate_limit_by_ip::{
        rate_limit_by_ip, RateLimitState, RateLimitStateForRoute,
    },
    proposer::api::ProposerApi,
    relay_data::{
        BidsCache, DataApi, DeliveredPayloadsCache, PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_API,
    },
    service::API_REQUEST_TIMEOUT,
};

pub type BuilderApiProd = BuilderApi<
    RedisCache,
    PostgresDatabaseService,
    OptimisticSimulator<RedisCache, PostgresDatabaseService>,
    GrpcGossiperClientManager,
>;

pub type ProposerApiProd = ProposerApi<
    RedisCache,
    PostgresDatabaseService,
    MultiBeaconClient<BeaconClient>,
    GrpcGossiperClientManager,
>;

pub type DataApiProd = DataApi<PostgresDatabaseService>;

pub type ConstraintsApiProd = ConstraintsApi<RedisCache, PostgresDatabaseService>;

pub fn build_router(
    router_config: &mut RouterConfig,
    builder_api: Arc<BuilderApiProd>,
    proposer_api: Arc<ProposerApiProd>,
    data_api: Arc<DataApiProd>,
    constraints_api: Arc<ConstraintsApiProd>,
    bids_cache: Arc<BidsCache>,
    delivered_payloads_cache: Arc<DeliveredPayloadsCache>,
) -> Router {
    router_config.resolve_condensed_routes();

    let mut rate_limits_per_route = HashMap::new();
    for route_info in &router_config.enabled_routes {
        if let Some(rate_limit) = route_info.rate_limit.as_ref() {
            rate_limits_per_route.insert(
                route_info.route.path(),
                RateLimitStateForRoute::new(
                    Duration::from_millis(rate_limit.limit_duration_ms),
                    rate_limit.max_requests,
                ),
            );
        }
    }
    let rate_limiting_state = RateLimitState::new(rate_limits_per_route);
    let mut router = Router::new().with_state(rate_limiting_state.clone());

    for route in router_config.enabled_routes.iter().map(|route_info| route_info.route) {
        match route {
            Route::GetValidators => {
                router = router.route(&route.path(), get(BuilderApiProd::get_validators));
            }
            Route::SubmitBlock => {
                router = router.route(&route.path(), post(BuilderApiProd::submit_block));
            }
            Route::SubmitBlockOptimistic => {
                router = router.route(&route.path(), post(BuilderApiProd::submit_block_v2));
            }
            Route::SubmitBlockWithProofs => {
                router =
                    router.route(&route.path(), post(BuilderApiProd::submit_block_with_proofs));
            }
            Route::SubmitHeader => {
                router = router.route(&route.path(), post(BuilderApiProd::submit_header));
            }
            Route::CancelBid => {
                router = router.route(&route.path(), post(BuilderApiProd::cancel_bid));
            }
            Route::GetTopBid => {
                router = router.route(&route.path(), get(BuilderApiProd::get_top_bid));
            }
            Route::GetBuilderConstraints => {
                router = router.route(&route.path(), get(BuilderApiProd::constraints));
            }
            Route::GetBuilderConstraintsStream => {
                router = router.route(&route.path(), get(BuilderApiProd::constraints_stream));
            }
            Route::GetBuilderDelegations => {
                router = router.route(&route.path(), get(BuilderApiProd::delegations));
            }
            Route::Status => {
                router = router.route(&route.path(), get(ProposerApiProd::status));
            }
            Route::RegisterValidators => {
                router = router.route(&route.path(), post(ProposerApiProd::register_validators));
            }
            Route::GetHeader => {
                router = router.route(&route.path(), get(ProposerApiProd::get_header));
            }
            Route::GetHeaderWithProofs => {
                router = router.route(&route.path(), get(ProposerApiProd::get_header_with_proofs));
            }
            Route::GetPayload => {
                router = router.route(&route.path(), post(ProposerApiProd::get_payload));
            }
            Route::ProposerPayloadDelivered => {
                router = router.route(&route.path(), get(DataApiProd::proposer_payload_delivered));
            }
            Route::BuilderBidsReceived => {
                router = router.route(
                    &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
                    get(DataApiProd::builder_bids_received),
                );
            }
            Route::ValidatorRegistration => {
                router = router.route(&route.path(), get(DataApiProd::validator_registration));
            }
            Route::SubmitBuilderConstraints => {
                router = router.route(&route.path(), post(ConstraintsApiProd::submit_constraints));
            }
            Route::DelegateSubmissionRights => {
                router = router.route(&route.path(), post(ConstraintsApiProd::delegate));
            }
            Route::RevokeSubmissionRights => {
                router = router.route(&route.path(), post(ConstraintsApiProd::revoke));
            }
            _ => {
                panic!("Route not implemented: {:?}, please add handling if there are new routes or resolve condensed routes before!", route);
            }
        }
    }

    // Add payload size limit
    router = router.layer(RequestBodyLimitLayer::new(MAX_PAYLOAD_LENGTH));

    // Add Rate-Limiting Layer
    router = router
        .route_layer(middleware::from_fn_with_state(rate_limiting_state.clone(), rate_limit_by_ip));

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
        .layer(Extension(data_api))
        .layer(Extension(constraints_api))
        .layer(Extension(bids_cache))
        .layer(Extension(delivered_payloads_cache));

    router
}
