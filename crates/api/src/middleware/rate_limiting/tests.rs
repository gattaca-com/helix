use crate::middleware::rate_limiting::rate_limit_by_ip::{
    rate_limit_by_ip, RateLimitState, RateLimitStateForRoute,
};
use axum::{middleware, routing::get, Router};
use serial_test::serial;
use std::{collections::HashMap, net::SocketAddr, time::Duration};
use tokio::sync::oneshot;

const ROUTE_NO_LIMIT: &str = "/test_without_limit";
const NO_LIMIT_RESPONSE: &str = "no limit";
const ROUTE_WITH_LIMIT: &str = "/test_with_limit";
const LIMIT_RESPONSE: &str = "with limit";
const ROUTE_WITH_HIGH_LIMIT: &str = "/test_with_high_limit";
const HIGH_LIMIT_RESPONSE: &str = "with high limit";

fn get_router() -> Router<()> {
    let mut rate_limits: HashMap<String, RateLimitStateForRoute> = HashMap::new();
    rate_limits.insert(
        ROUTE_WITH_LIMIT.to_string(),
        RateLimitStateForRoute::new(Duration::from_secs(5), 1),
    );
    rate_limits.insert(
        ROUTE_WITH_HIGH_LIMIT.to_string(),
        RateLimitStateForRoute::new(Duration::from_secs(10), 10),
    );
    let rate_limiting_state = RateLimitState::new(rate_limits);
    let mut app = Router::new();

    app = app
        .route(ROUTE_NO_LIMIT, get(|| async { NO_LIMIT_RESPONSE }))
        .route(ROUTE_WITH_LIMIT, get(|| async { LIMIT_RESPONSE }))
        .route(ROUTE_WITH_HIGH_LIMIT, get(|| async { HIGH_LIMIT_RESPONSE }))
        .route_layer(middleware::from_fn_with_state(rate_limiting_state.clone(), rate_limit_by_ip));

    app
}

async fn start_server() -> oneshot::Sender<()> {
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async {
        let router = get_router();
        // Start the server
        let listener: tokio::net::TcpListener =
            tokio::net::TcpListener::bind("0.0.0.0:4040").await.unwrap();
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async {
                rx.await.ok();
            })
            .await
            .unwrap();
    });

    tx
}

#[tokio::test]
#[serial]
async fn test_no_limit() {
    let tx = start_server().await;
    let url = format!("http://localhost:4040{}", ROUTE_NO_LIMIT);
    for _ in 0..11 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), NO_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_limit() {
    let tx = start_server().await;
    let url = format!("http://localhost:4040{}", ROUTE_WITH_LIMIT);
    let response = reqwest::get(url.clone()).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), LIMIT_RESPONSE);

    for _ in 0..11 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 429);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    let response = reqwest::get(url.clone()).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), LIMIT_RESPONSE);

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_high_limit() {
    let tx = start_server().await;
    let url = format!("http://localhost:4040{}", ROUTE_WITH_HIGH_LIMIT);
    for _ in 0..10 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), HIGH_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_high_limit_exceeded() {
    let tx = start_server().await;
    let url = format!("http://localhost:4040{}", ROUTE_WITH_HIGH_LIMIT);
    for _ in 0..10 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), HIGH_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let response = reqwest::get(url).await.unwrap();
    assert_eq!(response.status(), 429);

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_high_limit_reset() {
    let tx = start_server().await;
    let url = format!("http://localhost:4040{}", ROUTE_WITH_HIGH_LIMIT);
    for _ in 0..10 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), HIGH_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_secs(10)).await;

    for _ in 0..10 {
        let response = reqwest::get(url.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), HIGH_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_mixed_requests() {
    let tx = start_server().await;
    let url_no_limit = format!("http://localhost:4040{}", ROUTE_NO_LIMIT);
    let url_with_limit = format!("http://localhost:4040{}", ROUTE_WITH_LIMIT);
    let url_with_high_limit = format!("http://localhost:4040{}", ROUTE_WITH_HIGH_LIMIT);

    let response = reqwest::get(url_with_limit.clone()).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), LIMIT_RESPONSE);

    for _ in 0..10 {
        let response = reqwest::get(url_with_limit.clone()).await.unwrap();
        assert_eq!(response.status(), 429);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for _ in 0..11 {
        let response = reqwest::get(url_no_limit.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), NO_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for _ in 0..10 {
        let response = reqwest::get(url_with_high_limit.clone()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), HIGH_LIMIT_RESPONSE);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    let response = reqwest::get(url_with_limit.clone()).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), LIMIT_RESPONSE);

    // Shut down the server
    let _ = tx.send(());
}
