use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Router, extract::Extension, http::StatusCode, routing::get};
use helix_common::{Route, RouterConfig, ValidatorPreferences};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use moka::sync::Cache;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor};
use tracing::{error, info, warn};

use crate::{
    api::{BidsCache, BidsCacheV2, DataApi, DeliveredPayloadsCache, DeliveredPayloadsCacheV2},
    stats::SelectiveExpiry,
};

async fn status() -> StatusCode {
    StatusCode::OK
}

pub fn build_data_router(
    data_api: Arc<DataApi>,
    bids_cache: BidsCache,
    bids_cache_v2: BidsCacheV2,
    delivered_payloads_cache: DeliveredPayloadsCache,
    delivered_payloads_cache_v2: DeliveredPayloadsCacheV2,
    router_config: &RouterConfig,
) -> Router {
    let mut router = Router::new();
    let mut limiters = Vec::new();

    for route_info in &router_config.enabled_routes {
        let handler = match route_info.route {
            Route::Status => get(status),
            Route::ProposerPayloadDelivered => get(DataApi::proposer_payload_delivered),
            Route::ProposerPayloadDeliveredV2 => get(DataApi::proposer_payload_delivered_v2),
            Route::ProposerHeaderDelivered => get(DataApi::proposer_header_delivered),
            Route::BuilderBidsReceived => get(DataApi::builder_bids_received),
            Route::BuilderBidsReceivedV2 => get(DataApi::builder_bids_received_v2),
            Route::ValidatorRegistration => get(DataApi::validator_registration),
            Route::DataAdjustments => get(DataApi::data_adjustments),
            Route::MergedBlocks => get(DataApi::merged_blocks),
            r => {
                warn!("route {r:?} not supported by data API, skipping");
                continue;
            }
        };

        let path = route_info.route.path();

        let maybe_limited = if let Some(rate_limit) = route_info.rate_limit.as_ref() {
            let config = Arc::new(
                GovernorConfigBuilder::default()
                    .per_millisecond(rate_limit.replenish_ms)
                    .burst_size(rate_limit.burst_size)
                    .key_extractor(SmartIpKeyExtractor)
                    .finish()
                    .unwrap(),
            );
            limiters.push((config.limiter().clone(), path.clone()));
            handler.layer(GovernorLayer { config })
        } else {
            handler
        };

        router = router.route(&path, maybe_limited);
    }

    std::thread::spawn(move || {
        let interval = Duration::from_secs(60);
        loop {
            std::thread::sleep(interval);
            for (limiter, route) in &limiters {
                info!(size = limiters.len(), %route, "pruning rate limits");
                limiter.retain_recent();
            }
        }
    });

    router
        .layer(Extension(data_api))
        .layer(Extension(bids_cache))
        .layer(Extension(bids_cache_v2))
        .layer(Extension(delivered_payloads_cache))
        .layer(Extension(delivered_payloads_cache_v2))
}

pub async fn run_data_api(
    db: Arc<PostgresDatabaseService>,
    validator_preferences: Arc<ValidatorPreferences>,
    port: u16,
    router_config: RouterConfig,
) -> eyre::Result<()> {
    let data_api = Arc::new(DataApi::new(validator_preferences, db));

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

    let router = build_data_router(
        data_api,
        bids_cache,
        bids_cache_v2,
        delivered_payloads_cache,
        delivered_payloads_cache_v2,
        &router_config,
    );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!(port, "data API listening");
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("data API server exited"),
        Err(e) => error!("data API server error: {e}"),
    }
    Ok(())
}
