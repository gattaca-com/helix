use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Extension, Router, routing::get};
use helix_common::{
    ValidatorPreferences,
    api::{
        PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_ADJUSTMENTS, PATH_DATA_API, PATH_MERGED_BLOCKS,
        PATH_PROPOSER_HEADER_DELIVERED, PATH_PROPOSER_PAYLOAD_DELIVERED,
        PATH_VALIDATOR_REGISTRATION,
    },
};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use moka::sync::Cache;
use tracing::{error, info};

use crate::{
    api::{BidsCache, DataApi, DeliveredPayloadsCache},
    stats::SelectiveExpiry,
};

pub fn build_data_router(
    data_api: Arc<DataApi>,
    bids_cache: BidsCache,
    delivered_payloads_cache: DeliveredPayloadsCache,
) -> Router {
    Router::new()
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}"),
            get(DataApi::proposer_payload_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_PROPOSER_HEADER_DELIVERED}"),
            get(DataApi::proposer_header_delivered),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            get(DataApi::builder_bids_received),
        )
        .route(
            &format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            get(DataApi::validator_registration),
        )
        .route(&format!("{PATH_DATA_API}{PATH_DATA_ADJUSTMENTS}"), get(DataApi::data_adjustments))
        .route(&format!("{PATH_DATA_API}{PATH_MERGED_BLOCKS}"), get(DataApi::merged_blocks))
        .layer(Extension(data_api))
        .layer(Extension(bids_cache))
        .layer(Extension(delivered_payloads_cache))
}

/// Run a standalone data API server.
///
/// Binds on `0.0.0.0:{port}`. Caller is responsible for setting up a tokio runtime and
/// initialising the database service before calling this.
pub async fn run_data_api(
    db: Arc<PostgresDatabaseService>,
    validator_preferences: Arc<ValidatorPreferences>,
    port: u16,
) -> eyre::Result<()> {
    let data_api = Arc::new(DataApi::new(validator_preferences, db));

    let bids_cache: BidsCache =
        Cache::builder().time_to_idle(Duration::from_secs(12)).max_capacity(10_000).build();

    let delivered_payloads_cache: DeliveredPayloadsCache = Cache::builder()
        .expire_after(SelectiveExpiry)
        .time_to_idle(Duration::from_secs(12))
        .max_capacity(10_000)
        .build();

    let router = build_data_router(data_api, bids_cache, delivered_payloads_cache);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!(port, "data API listening");
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("data API server exited"),
        Err(e) => error!("data API server error: {e}"),
    }
    Ok(())
}
