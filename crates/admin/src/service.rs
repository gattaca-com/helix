use std::{net::SocketAddr, sync::Arc};

use axum::{
    Extension, Router,
    http::StatusCode,
    routing::{get, post},
};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tracing::{error, info};

use crate::{assets, config::AdminConfig, handlers, relay_client::RelayAdminClient};

async fn status() -> StatusCode {
    StatusCode::OK
}

pub fn build_admin_router(
    db: Arc<PostgresDatabaseService>,
    validator_preferences: Arc<helix_common::ValidatorPreferences>,
    relay_client: RelayAdminClient,
    admin_token: &str,
) -> Router {
    let api_v1 = Router::new()
        .route("/overview", get(handlers::overview))
        .route("/builders", get(handlers::builders))
        .route("/demotions", get(handlers::demotions))
        .route("/payloads", get(handlers::payloads))
        .route("/bids", get(handlers::bids))
        .route("/validators/{pubkey}", get(handlers::validator_registration))
        .route("/trusted-proposers", get(handlers::trusted_proposers))
        .route("/adjustments/status", get(handlers::adjustments_status))
        .route(
            "/actions/killswitch",
            post(handlers::enable_kill_switch).delete(handlers::disable_kill_switch),
        )
        .route("/actions/builders/{pubkey}/demote", post(handlers::demote_builder))
        .route("/actions/builders/{pubkey}/promote", post(handlers::promote_builder))
        .route("/actions/adjustments/disable", post(handlers::disable_adjustments))
        .layer(ValidateRequestHeaderLayer::bearer(admin_token));

    Router::new()
        .route("/api/status", get(status))
        .nest("/api/v1", api_v1)
        .fallback(get(assets::static_handler))
        .layer(Extension(db))
        .layer(Extension(validator_preferences))
        .layer(Extension(relay_client))
}

pub async fn run_admin_api(
    db: Arc<PostgresDatabaseService>,
    config: AdminConfig,
    admin_token: String,
    relay_admin_token: String,
) -> eyre::Result<()> {
    let relay_client = RelayAdminClient::new(config.relay_admin_url.as_str(), relay_admin_token);
    let validator_preferences = Arc::new(config.validator_preferences);

    let router = build_admin_router(db, validator_preferences, relay_client, &admin_token);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.api_port)).await?;
    info!(port = config.api_port, "admin API listening");
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("admin API server exited"),
        Err(e) => error!("admin API server error: {e}"),
    }
    Ok(())
}
