use std::{net::SocketAddr, sync::Arc};

use axum::{routing::any, Extension, Router};
use helix_common::{chain_info::ChainInfo, signing::RelaySigningContext};
use helix_p2p::{messages::InclusionListMessage, P2PApi};
use helix_types::BlsKeypair;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(4040);
    let peer_addresses: Vec<SocketAddr> = args.map(|s| s.parse().unwrap()).collect();

    let keypair = BlsKeypair::random();
    let chain_info = Arc::new(ChainInfo::for_hoodi());
    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info));

    let p2p_api = P2PApi::new(peer_addresses, relay_signing_context).await;

    let router = Router::new()
        .route("/relay/v1/p2p", any(P2PApi::p2p_connect))
        .layer(Extension(p2p_api.clone()));

    tokio::spawn(async move {
        loop {
            let message = InclusionListMessage { slot: 42 }.into();
            p2p_api.broadcast(message);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    println!("Listening on ws://127.0.0.1:{port}/relay/v1/p2p");

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}
