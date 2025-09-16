mod consensus;
pub(crate) mod service;

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

    use alloy_consensus::{TxEip1559, TxEnvelope};
    use alloy_primitives::Signature;
    use alloy_rlp::Encodable as _;
    use axum::{routing::any, Extension, Router};
    use helix_common::{
        api::builder_api::InclusionList, chain_info::ChainInfo, signing::RelaySigningContext,
        utils::utcnow_sec, RelayNetworkConfig, RelayNetworkPeerConfig,
    };
    use helix_types::{BlsKeypair, BlsSecretKey, Transaction};
    use rand::{rngs::SmallRng, seq::IndexedRandom, Rng as _, SeedableRng};
    use tokio::task::JoinSet;
    use tracing::{error, info};
    use tree_hash::TreeHash;

    use crate::{inclusion_lists::consensus::INCLUSION_LIST_MAX_BYTES, RelayNetworkApi};

    const RELAY_CONNECT_PATH: &str = "/relay/v1/network";

    fn create_random_tx(id: u64, rng: &mut SmallRng) -> Transaction {
        // Pad with random bytes to randomize tx length
        let n_bytes = rng.random_range(0..300);
        let random_bytes = rng.random_iter::<u8>().take(n_bytes).collect();

        // Use arbitrary values since we don't check them
        let tx = TxEip1559 {
            chain_id: 0,
            nonce: 0,
            // Use id in some field to differentiate the txs
            gas_limit: id,
            max_fee_per_gas: 42,
            max_priority_fee_per_gas: 42,
            to: alloy_primitives::TxKind::Call(Default::default()),
            value: Default::default(),
            access_list: Default::default(),
            input: random_bytes,
        };
        // Use an empty signature
        let tx = TxEnvelope::new_unhashed(
            tx.into(),
            Signature::new(Default::default(), Default::default(), Default::default()),
        );
        let mut buf = vec![];
        tx.encode(&mut buf);
        Transaction(buf.into())
    }

    async fn wait_until_next_slot(chain_info: &ChainInfo) {
        let slot_duration = Duration::from_secs(chain_info.seconds_per_slot());

        // Unless we are at the start of a slot, sleep until the start of the next one
        let sleep_duration = slot_duration.saturating_sub(
            chain_info.duration_into_slot(chain_info.current_slot()).unwrap_or_default(),
        );

        tokio::time::sleep(sleep_duration).await;
    }

    fn start_network_relay(
        join_set: &mut JoinSet<()>,
        chain_info: Arc<ChainInfo>,
        port: u16,
        private_key: BlsSecretKey,
        network_config: RelayNetworkConfig,
    ) -> Arc<RelayNetworkApi> {
        let keypair = BlsKeypair::from_components(private_key.public_key(), private_key);
        let pubkey = keypair.pk.clone();
        let relay_signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));

        let network_api = RelayNetworkApi::new(network_config, relay_signing_context);

        let router = Router::new()
            .route(RELAY_CONNECT_PATH, any(RelayNetworkApi::connect))
            .layer(Extension(network_api.clone()));

        info!("Listening on ws://127.0.0.1:{port}{RELAY_CONNECT_PATH} with pubkey: {pubkey}");

        join_set.spawn(async move {
            let listener =
                tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
            match axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                Ok(_) => info!("Server exited successfully"),
                Err(e) => error!("Server exited with error: {e}"),
            }
        });
        network_api
    }

    #[tokio::test]
    async fn multi_relay_inclusion_lists_integration_test() {
        // Enable to produce logs
        // let _guard =
        //     helix_common::utils::init_tracing_log(&Default::default(), "", Default::default());

        let n_peers = 5;
        let n_slots = 4;
        // Approximate max number of bytes in the mempool per slot.
        // ILs will be built from random samples of the mempool.
        let mempool_bytes_per_slot = 2 * INCLUSION_LIST_MAX_BYTES;
        // Use seed for reproducibility
        let seed = utcnow_sec();

        let port_start = 4050_u16;

        let mut network_config = RelayNetworkConfig {
            is_enabled: true,
            peers: vec![],
            cutoff_1_ms: 2000,
            cutoff_2_ms: 4000,
        };
        let ports = (port_start..).take(n_peers).collect::<Vec<_>>();
        let keypairs = (0..n_peers).map(|_| BlsKeypair::random()).collect::<Vec<_>>();

        // We generate a single config, since peers won't connect to themselves
        network_config.peers = (0..n_peers)
            .map(|i| RelayNetworkPeerConfig {
                url: format!("ws://127.0.0.1:{}{RELAY_CONNECT_PATH}", ports[i]),
                pubkey: keypairs[i].pk.serialize().into(),
            })
            .collect();

        let chain_info = Arc::new(ChainInfo::for_hoodi());

        let mut apis_joinset = JoinSet::new();
        let mut network_apis = Vec::with_capacity(n_peers);

        for i in 0..n_peers {
            let network_api = start_network_relay(
                &mut apis_joinset,
                chain_info.clone(),
                ports[i],
                keypairs[i].sk.clone(),
                network_config.clone(),
            );
            network_apis.push(network_api);
        }

        // Wait a bit for all peers to connect with each other
        tokio::time::sleep(Duration::from_secs(8)).await;

        let mut rng = SmallRng::seed_from_u64(seed);
        info!(%seed, "Using seed for random number generation");

        let slot_duration = Duration::from_secs(chain_info.seconds_per_slot());

        info!("Sleeping until the start of the next slot");
        wait_until_next_slot(&chain_info).await;
        info!(%n_slots, "Starting simulation");

        for slot in 0..n_slots {
            let real_slot = chain_info.current_slot();

            info!(%slot, "Generating transactions");

            let mut remaining_bytes = mempool_bytes_per_slot;
            let all_txs: Vec<_> = (0..)
                .map(|id| create_random_tx(id, &mut rng))
                .take_while(|tx| {
                    if remaining_bytes < tx.len() {
                        false
                    } else {
                        remaining_bytes -= tx.len();
                        true
                    }
                })
                .collect();

            info!(%slot, count=%all_txs.len(), "Generated transactions");

            let mut join_set = JoinSet::new();

            // Send the transactions to all peers
            for network_api in &network_apis {
                let mut remaining_bytes = INCLUSION_LIST_MAX_BYTES;
                let txs: Vec<Transaction> = all_txs
                    .choose_multiple(&mut rng, all_txs.len())
                    .take_while(|tx| {
                        if remaining_bytes < tx.len() {
                            false
                        } else {
                            remaining_bytes -= tx.len();
                            true
                        }
                    })
                    .cloned()
                    .collect();
                let inclusion_list = InclusionList { txs: txs.into() };
                // Share IL in the background
                let network_api = network_api.clone();
                join_set.spawn(async move {
                    network_api.share_inclusion_list(real_slot.into(), inclusion_list).await
                });
            }
            info!(%slot, "Sent transactions to each process");

            let Ok(results) = tokio::time::timeout(slot_duration, join_set.join_all()).await else {
                error!(%slot, %seed, "Timed out waiting for inclusion lists");
                panic!("Timed out waiting for inclusion lists");
            };
            info!(%slot, "Got inclusion lists from all peers");

            assert_eq!(results.len(), n_peers);
            let mut results_map = HashMap::new();
            for il in results {
                let hash = il.as_ref().map(TreeHash::tree_hash_root).unwrap_or_default();
                results_map.entry(hash).and_modify(|(c, _)| *c += 1).or_insert((1, il));
            }
            let n_unique = results_map.len();
            assert_eq!(n_unique, 1, "Expected only one inclusion list");
            let (_hash, (n, _il)) = results_map.into_iter().next().unwrap();
            assert_eq!(n, n_peers, "Expected all peers to return an inclusion list");

            info!(%slot, %n_slots, "All peers returned the same inclusion list. Waiting until next slot");
            wait_until_next_slot(&chain_info).await;
        }

        apis_joinset.shutdown().await;
    }
}
