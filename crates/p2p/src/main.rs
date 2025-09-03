use std::net::SocketAddr;

use axum::{routing::any, Extension, Router};
use helix_p2p::P2pApi;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    let router =
        Router::new().route("/relay/v1/p2p", any(P2pApi::p2p_connect)).layer(Extension(P2pApi));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:4040").await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}
