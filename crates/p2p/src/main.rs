use std::{net::SocketAddr, sync::Arc};

use axum::{routing::any, Extension, Router};
use helix_common::{chain_info::ChainInfo, signing::RelaySigningContext, P2PPeerConfig};
use helix_p2p::{messages::InclusionListMessage, P2PApi};
use helix_types::{BlsKeypair, BlsSecretKey};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

#[derive(Serialize, Deserialize)]
struct Config {
    port: u16,
    peer_configs: Vec<P2PPeerConfig>,
    private_key: String,
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let config_path = args.next().unwrap();

    let file_str = std::fs::read_to_string(config_path).unwrap();
    let config: Config = serde_json::from_str(&file_str).unwrap();
    let port = config.port;
    let peer_configs = config.peer_configs;

    let private_key_bytes = hex::decode(&config.private_key).unwrap();
    let private_key = BlsSecretKey::deserialize(&private_key_bytes).unwrap();

    let keypair = BlsKeypair::from_components(private_key.public_key(), private_key);
    let pubkey = keypair.pk.clone();
    let chain_info = Arc::new(ChainInfo::for_hoodi());
    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info));

    let p2p_api = P2PApi::new(peer_configs, relay_signing_context).await;

    let router = Router::new()
        .route("/relay/v1/p2p", any(P2PApi::p2p_connect))
        .layer(Extension(p2p_api.clone()));

    tokio::spawn(async move {
        loop {
            let message = InclusionListMessage { slot: 42, inclusion_list: vec![].into() }.into();
            p2p_api.broadcast(message);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    println!("Listening on ws://127.0.0.1:{port}/relay/v1/p2p with pubkey: {pubkey}");

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}
