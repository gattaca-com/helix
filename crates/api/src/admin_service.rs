use std::{net::SocketAddr, sync::Arc};

use axum::{http::StatusCode, response::IntoResponse, routing::post, Extension, Router};
use helix_common::{local_cache::LocalCache, RelayConfig};
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tracing::{error, info};

pub async fn run_admin_service(auctioneer: Arc<LocalCache>, config: RelayConfig) {
    let router = Router::new()
        .route("/admin/v1/killswitch", post(enable_kill_switch).delete(disable_kill_switch))
        .layer(Extension(auctioneer))
        .layer(ValidateRequestHeaderLayer::bearer(&config.admin_token));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4050").await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod test {
    use std::sync::Arc;

    use helix_common::{config, local_cache::LocalCache};
    use serial_test::serial;

    use crate::admin_service::run_admin_service;

    #[tokio::test]
    #[serial]
    async fn test_admin_service() {
        let auctioneer = Arc::new(LocalCache::new_test());

        let mut config = config::RelayConfig::default();
        config.admin_token = "test_token".into();
        tokio::spawn(run_admin_service(auctioneer.clone(), config));
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
        let auctioneer = Arc::new(LocalCache::new_test());

        let mut config = config::RelayConfig::default();
        config.admin_token = "test_token".into();
        tokio::spawn(run_admin_service(auctioneer.clone(), config));
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
