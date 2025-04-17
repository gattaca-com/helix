use std::{sync::Arc, time::Duration};

use axum::{
    error_handling::HandleErrorLayer,
    http::StatusCode,
    middleware,
    routing::{get, post},
    Extension, Router,
};
use helix_common::{
    metadata_provider::MetadataProvider, utils::extract_request_id, Route, RouterConfig,
};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
use hyper::{HeaderMap, Uri};
use tower::{timeout::TimeoutLayer, BoxError, ServiceBuilder};
use tower_governor::{
    governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor, GovernorLayer,
};
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
};
use tracing::{info, warn};

use crate::{
    builder::{
        api::{BuilderApi, MAX_PAYLOAD_LENGTH},
        multi_simulator::MultiSimulator,
        optimistic_simulator::OptimisticSimulator,
    },
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    middleware::{inner_metrics_middleware, outer_metrics_middleware},
    proposer::api::ProposerApi,
    relay_data::{BidsCache, DataApi, DeliveredPayloadsCache},
    service::API_REQUEST_TIMEOUT,
};

pub type BuilderApiProd<MP> = BuilderApi<
    RedisCache,
    PostgresDatabaseService,
    MultiSimulator<OptimisticSimulator<RedisCache, PostgresDatabaseService>>,
    GrpcGossiperClientManager,
    MP,
>;

pub type ProposerApiProd<MP> =
    ProposerApi<RedisCache, PostgresDatabaseService, GrpcGossiperClientManager, MP>;

pub type DataApiProd = DataApi<PostgresDatabaseService>;

pub fn build_router<MP: MetadataProvider>(
    router_config: &mut RouterConfig,
    builder_api: Arc<BuilderApiProd<MP>>,
    proposer_api: Arc<ProposerApiProd<MP>>,
    data_api: Arc<DataApiProd>,
    bids_cache: Arc<BidsCache>,
    delivered_payloads_cache: Arc<DeliveredPayloadsCache>,
) -> Router {
    router_config.resolve_condensed_routes();

    let mut router = Router::new();
    let mut limiters = Vec::new();

    for route_info in router_config.enabled_routes.iter() {
        let method = match route_info.route {
            Route::GetValidators => get(BuilderApiProd::<MP>::get_validators),
            Route::SubmitBlock => post(BuilderApiProd::<MP>::submit_block),
            Route::SubmitBlockOptimistic => post(BuilderApiProd::<MP>::submit_block_v2),
            Route::SubmitHeader => post(BuilderApiProd::<MP>::submit_header),
            Route::GetTopBid => get(BuilderApiProd::<MP>::get_top_bid),
            Route::Status => get(ProposerApiProd::<MP>::status),
            Route::RegisterValidators => post(ProposerApiProd::<MP>::register_validators),
            Route::GetHeader => get(ProposerApiProd::<MP>::get_header),
            Route::GetPayload => post(ProposerApiProd::<MP>::get_payload),
            Route::ProposerPayloadDelivered => get(DataApiProd::proposer_payload_delivered),
            Route::BuilderBidsReceived => get(DataApiProd::builder_bids_received),
            Route::ValidatorRegistration => get(DataApiProd::validator_registration),
            Route::SubmitHeaderV3 => post(BuilderApiProd::<MP>::submit_header_v3),
            _ => {
                panic!("Route not implemented: {:?}, please add handling if there are new routes or resolve condensed routes before!", route_info.route);
            }
        };

        let maybe_limited = if let Some(rate_limit) = route_info.rate_limit.as_ref() {
            let config = Arc::new(
                GovernorConfigBuilder::default()
                    .per_millisecond(rate_limit.replenish_ms)
                    .burst_size(rate_limit.burst_size)
                    .key_extractor(SmartIpKeyExtractor)
                    .finish()
                    .unwrap(),
            );

            let governor_limiter = config.limiter().clone();
            limiters.push((governor_limiter, route_info.route.path()));

            method.layer(GovernorLayer { config })
        } else {
            method
        };

        router = router.route(&route_info.route.path(), maybe_limited);
    }

    // periodically prune rate limits
    std::thread::spawn(move || {
        let interval = Duration::from_secs(60);
        loop {
            std::thread::sleep(interval);
            for (limiter, route) in limiters.iter() {
                info!(size = limiters.len(), %route, "pruning rate limits");
                limiter.retain_recent();
            }
        }
    });

    // add layers, split in two to make the traits happy, layers are applied bottom to top across
    // router.layer, but in order for each of the two
    router = router.layer(
        ServiceBuilder::new()
            .layer(middleware::from_fn(inner_metrics_middleware)) // body size
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid)) // request ids
            .layer(PropagateRequestIdLayer::x_request_id()) // propagate request id
            .layer(HandleErrorLayer::new(|uri: Uri, headers: HeaderMap, e: BoxError| async move {
                let request_id = extract_request_id(&headers);
                warn!(uri = %uri.path(), %request_id, "request timed out {:?}", e);
                StatusCode::REQUEST_TIMEOUT
            })) // timeout
            .layer(TimeoutLayer::new(API_REQUEST_TIMEOUT)),
    );

    // this is applied first
    router = router.layer(
        ServiceBuilder::new()
            .layer(middleware::from_fn(outer_metrics_middleware)) // status and full latency
            .layer(RequestBodyLimitLayer::new(MAX_PAYLOAD_LENGTH)), // streaming body limit
    );

    // Add Extension layers
    router = router
        .layer(Extension(builder_api))
        .layer(Extension(proposer_api))
        .layer(Extension(data_api))
        .layer(Extension(bids_cache))
        .layer(Extension(delivered_payloads_cache));

    router
}
