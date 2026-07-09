use std::{net::SocketAddr, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use helix_common::local_cache::LocalCache;
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tracing::{error, info};

pub async fn run_admin_service(
    auctioneer: Arc<LocalCache>,
    db: Arc<PostgresDatabaseService>,
    admin_token: String,
) {
    let router = Router::new()
        .route("/admin/v1/status", get(status))
        .route("/admin/v1/killswitch", post(enable_kill_switch).delete(disable_kill_switch))
        .route("/admin/v1/builders/{pubkey}/demote", post(demote_builder))
        .route("/admin/v1/builders/{pubkey}/promote", post(promote_builder))
        .route("/admin/v1/adjustments/disable", post(disable_adjustments))
        .layer(Extension(auctioneer))
        .layer(Extension(db))
        .layer(ValidateRequestHeaderLayer::bearer(&admin_token));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4050").await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}

async fn status(
    Extension(auctioneer): Extension<Arc<LocalCache>>,
) -> Result<impl IntoResponse, StatusCode> {
    Ok(Json(serde_json::json!({ "kill_switch_enabled": auctioneer.kill_switch_enabled() })))
}

async fn enable_kill_switch(
    Extension(auctioneer): Extension<Arc<LocalCache>>,
) -> Result<impl IntoResponse, StatusCode> {
    auctioneer.enable_kill_switch();
    info!("Kill switch enabled");
    Ok((StatusCode::NO_CONTENT, ()))
}

async fn disable_kill_switch(
    Extension(auctioneer): Extension<Arc<LocalCache>>,
) -> Result<impl IntoResponse, StatusCode> {
    auctioneer.disable_kill_switch();
    info!("Kill switch disabled");
    Ok((StatusCode::NO_CONTENT, ()))
}

#[derive(Deserialize)]
struct DemoteRequest {
    reason: Option<String>,
}

async fn demote_builder(
    Extension(auctioneer): Extension<Arc<LocalCache>>,
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
    body: Option<Json<DemoteRequest>>,
) -> Result<impl IntoResponse, StatusCode> {
    if auctioneer.get_builder_info(&pubkey).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    auctioneer.demote_builder(&pubkey);
    info!(%pubkey, "builder demoted via admin API");

    let reason = body
        .and_then(|Json(req)| req.reason)
        .unwrap_or_else(|| "manual demotion via admin API".to_string());

    // slot 0 / zero block hash mark the demotion record as manual
    if let Err(err) = db.db_demote_builder(0, &pubkey, &Default::default(), reason).await {
        error!(%pubkey, %err, "failed to persist builder demotion");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok((StatusCode::NO_CONTENT, ()))
}

async fn promote_builder(
    Extension(auctioneer): Extension<Arc<LocalCache>>,
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
) -> Result<impl IntoResponse, StatusCode> {
    if auctioneer.get_builder_info(&pubkey).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    auctioneer.promote_builder(&pubkey);
    info!(%pubkey, "builder promoted via admin API");

    if let Err(err) = db.db_promote_builder(&pubkey).await {
        error!(%pubkey, %err, "failed to persist builder promotion");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok((StatusCode::NO_CONTENT, ()))
}

async fn disable_adjustments(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
) -> Result<impl IntoResponse, StatusCode> {
    if let Err(err) = db.disable_adjustments().await {
        error!(%err, "failed to disable adjustments");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    info!("Adjustments disabled via admin API");
    Ok((StatusCode::NO_CONTENT, ()))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod test {
    use std::sync::Arc;

    use helix_common::local_cache::LocalCache;
    use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
    use serial_test::serial;

    use crate::api::admin_service::run_admin_service;

    #[tokio::test]
    #[serial]
    async fn test_admin_service() {
        let auctioneer = Arc::new(LocalCache::new());
        let db = Arc::new(PostgresDatabaseService::default());

        let admin_token = "test_token".into();
        tokio::spawn(run_admin_service(auctioneer.clone(), db, admin_token));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // wait for server to start
        let client = reqwest::Client::new();

        let response = client
            .post("http://localhost:4050/admin/v1/killswitch")
            .bearer_auth("test_token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert!(auctioneer.kill_switch_enabled());

        let response = client
            .get("http://localhost:4050/admin/v1/status")
            .bearer_auth("test_token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let status: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status["kill_switch_enabled"], true);

        let response = client
            .delete("http://localhost:4050/admin/v1/killswitch")
            .bearer_auth("test_token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert!(!auctioneer.kill_switch_enabled());
    }

    #[tokio::test]
    #[serial]
    async fn test_admin_service_unauthorized() {
        let auctioneer = Arc::new(LocalCache::new());
        let db = Arc::new(PostgresDatabaseService::default());

        let admin_token = "test_token".into();
        tokio::spawn(run_admin_service(auctioneer.clone(), db, admin_token));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // wait for server to start
        let client = reqwest::Client::new();

        let response =
            client.get("http://localhost:4050/admin/v1/killswitch/enable").send().await.unwrap();
        assert_eq!(response.status(), 401);
        assert!(!auctioneer.kill_switch_enabled());

        let response =
            client.get("http://localhost:4050/admin/v1/killswitch/disable").send().await.unwrap();
        assert_eq!(response.status(), 401);
        assert!(!auctioneer.kill_switch_enabled());
    }
}
