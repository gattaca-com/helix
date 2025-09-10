use std::{net::SocketAddr, sync::Arc, time::Duration};

use alloy_consensus::{TxEip1559, TxEnvelope};
use alloy_primitives::Signature;
use alloy_rlp::Encodable;
use axum::{routing::any, Extension, Router};
use helix_common::{
    api::builder_api::InclusionList, chain_info::ChainInfo, signing::RelaySigningContext,
    utils::init_tracing_log, P2PConfig, P2PPeerConfig,
};
use helix_p2p::P2PApi;
use helix_types::{BlsKeypair, BlsSecretKey, Transaction};
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
    let _guard = init_tracing_log(&Default::default(), "local", "P2PTest".to_string());
    let mut args = std::env::args().skip(1);
    let config_path = args.next().unwrap();

    let file_str = std::fs::read_to_string(config_path).unwrap();
    let config: Config = serde_json::from_str(&file_str).unwrap();
    let port = config.port;

    let p2p_config = P2PConfig { peers: config.peer_configs, ..P2PConfig::default() };
    let cutoff_time_1 = Duration::from_millis(p2p_config.cutoff_1_ms);

    let private_key_bytes = hex::decode(&config.private_key).unwrap();
    let private_key = BlsSecretKey::deserialize(&private_key_bytes).unwrap();

    let keypair = BlsKeypair::from_components(private_key.public_key(), private_key);
    let pubkey = keypair.pk.clone();
    let chain_info = Arc::new(ChainInfo::for_hoodi());
    let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));

    let p2p_api = P2PApi::new(p2p_config, relay_signing_context);

    let router = Router::new()
        .route("/relay/v1/p2p", any(P2PApi::p2p_connect))
        .layer(Extension(p2p_api.clone()));

    tokio::spawn(async move {
        let mut i = port as u64;
        loop {
            let tx = TxEip1559 {
                chain_id: 5,
                nonce: port.into(),
                gas_limit: i,
                max_fee_per_gas: 42,
                max_priority_fee_per_gas: 42,
                to: alloy_primitives::TxKind::Call(Default::default()),
                value: Default::default(),
                access_list: Default::default(),
                input: vec![0u8; port as usize].into(),
            };
            let tx = TxEnvelope::new_unhashed(
                tx.into(),
                Signature::new(Default::default(), Default::default(), Default::default()),
            );
            let mut buf = vec![];
            tx.encode(&mut buf);
            let txs = vec![Transaction(buf.into())].into();

            let slot_duration = Duration::from_secs(chain_info.seconds_per_slot());

            // Sleep until the start of the next slot
            let sleep_duration = slot_duration.saturating_sub(
                chain_info.duration_into_slot(chain_info.current_slot()).unwrap_or_default(),
            );
            if sleep_duration < cutoff_time_1 {
                tokio::time::sleep(sleep_duration).await;
            }

            let slot = chain_info.current_slot();

            let mut sleeper = core::pin::pin!(tokio::time::sleep(slot_duration));
            tokio::select! {
                il = p2p_api.share_inclusion_list(slot.into(), InclusionList { txs }) => {
                    let Some(il) = il else {
                        error!("Failed to get inclusion list");
                        continue;
                    };
                    let process = &il.txs[0][10] - 200;
                    info!("Got inclusion list from p{process:?}");
                    sleeper.await;
                },
                _ = &mut sleeper => {
                    error!("Timed out waiting for inclusion list");
                }
            }
            i += 42;
            i = i.wrapping_mul(3);
            i ^= 0xf0f0f0f0f0;
        }
    });

    println!("Listening on ws://127.0.0.1:{port}/relay/v1/p2p with pubkey: {pubkey}");

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
    match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
        Ok(_) => info!("Server exited successfully"),
        Err(e) => error!("Server exited with error: {e}"),
    }
}
