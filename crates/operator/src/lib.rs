use std::{
    fmt::{Display, Formatter},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::U256;
use async_channel::{Receiver, RecvError, SendError, Sender, TryRecvError, TrySendError, bounded};
use helix_common::{
    OperatorConfig, OperatorP2pMode,
    alerts::{AlertManager, format_demotion_alert},
    local_cache::LocalCache,
    utils::utcnow_ms,
};
use helix_database::{PostgresDatabaseService, handle::DbHandle};
use helix_types::{BuilderCollateral, Operator, OperatorMessage, Payload};
use libp2p::{BehaviourBuilderError, TransportError, identity::Keypair, multiaddr};
use thiserror::Error;
use tokio::task::AbortHandle;

mod pubsub;
mod utils;

pub use libp2p::identity::Keypair as OperatorKeypair;
pub use utils::keypair_from_bytes;

use crate::utils::load_operator_keypair;

#[derive(Debug, Error)]
pub enum OperatorError {
    MultiaddrParseError(#[from] multiaddr::Error),
    SwarmNetworkError(#[from] TransportError<std::io::Error>),
    SwarmBuildError(#[from] BehaviourBuilderError),
    MessageSendError(#[from] SendError<OperatorMessage>),
    MessageTrySendError(#[from] TrySendError<OperatorMessage>),
    MessageRecvError(#[from] RecvError),
    MessageTryRecvError(#[from] TryRecvError),
}

impl Display for OperatorError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        f.write_fmt(format_args!("{self:?}"))
    }
}

/// Handle to operator pubsub.
pub struct OperatorPubSub {
    outgoing_msgs: Sender<OperatorMessage>,
    incoming_msgs: Receiver<(Operator, OperatorMessage)>,
    task_handle: AbortHandle,
}

impl Drop for OperatorPubSub {
    fn drop(&mut self) {
        self.task_handle.abort();
    }
}

impl OperatorPubSub {
    pub fn new(
        quic_port: u16,
        local_keypair: Keypair,
        operators: Vec<Operator>,
        mode: OperatorP2pMode,
    ) -> Self {
        let (outgoing_msgs, out_recv) = bounded(128);
        let (in_send, incoming_msgs) = bounded(128);

        // p2p task.
        let handle = tokio::spawn(pubsub::run_operator_connection(
            quic_port,
            local_keypair,
            operators,
            out_recv,
            in_send,
            mode,
            #[cfg(test)]
            None,
        ));

        Self { outgoing_msgs, incoming_msgs, task_handle: handle.abort_handle() }
    }

    pub async fn send(&self, msg: OperatorMessage) -> Result<(), OperatorError> {
        Ok(self.outgoing_msgs.send(msg).await?)
    }

    pub fn try_send(&self, msg: OperatorMessage) -> Result<(), OperatorError> {
        Ok(self.outgoing_msgs.try_send(msg)?)
    }

    pub async fn recv(&self) -> Result<(Operator, OperatorMessage), OperatorError> {
        Ok(self.incoming_msgs.recv().await?)
    }

    pub fn try_recv(&self) -> Result<Option<(Operator, OperatorMessage)>, OperatorError> {
        match self.incoming_msgs.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn spawn_operator_connection<F>(
    config: OperatorConfig,
    loaded: Arc<AtomicBool>,
    local_cache: Arc<LocalCache>,
    db_handle: DbHandle,
    db_service: Arc<PostgresDatabaseService>,
    failsafe_triggered: Arc<AtomicBool>,
    alert_manager: Arc<AlertManager>,
    payload_handler: F,
) -> Arc<OperatorPubSub>
where
    F: Fn(Payload) + Send + Sync + 'static,
{
    // if there is `OperatorConfig`, then operator key is expected.
    let operator_keypair = load_operator_keypair();
    let operator_group = config.operator_group.map(|s| s.as_bytes().to_vec());
    let operator_pubsub = Arc::new(OperatorPubSub::new(
        config.quic_port,
        operator_keypair,
        config.operators,
        config.mode,
    ));

    // spawn a task to load initial db state
    tokio::spawn({
        let pubsub = operator_pubsub.clone();
        let cache = local_cache.clone();
        let group = operator_group.clone();
        async move {
            // wait for database load to complete.
            while !loaded.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let now = utcnow_ms();
            for (builder_pubkey, builder_info) in cache.all_builder_infos_local_collateral_only() {
                if builder_info.collateral > U256::ZERO {
                    let _ = pubsub
                        .send(OperatorMessage::Collateral(BuilderCollateral {
                            ts_ms: now,
                            slot: 0,
                            builder_pubkey,
                            collateral_wei: builder_info.collateral.to(),
                            operator_group: group.clone(),
                        }))
                        .await;
                }
            }

            // Load existing demotions
            match db_service.load_builder_demotions().await {
                Ok(demotions) => {
                    for demotion in demotions {
                        let _ = pubsub.send(OperatorMessage::Demotion(demotion)).await;
                    }
                }
                Err(e) => {
                    tracing::error!(?e, "failed to laod demotions from DB. Operators not updated.");
                }
            }
        }
    });

    // Spawn a task to process remote messages and periodically check for collateral updates.
    tokio::spawn({
        let pubsub = operator_pubsub.clone();
        async move {
            let mut collateral_resync = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                tokio::select! {
                    result = pubsub.recv() => {
                        let Ok((operator, msg)) = result else {
                            continue;
                        };
                        match msg {
                            OperatorMessage::Demotion(demotion) => {
                                if local_cache.demote_builder(&demotion.builder_pubkey) {
                                    let builder_id = local_cache
                                        .get_builder_info(&demotion.builder_pubkey)
                                        .and_then(|info| info.builder_id)
                                        .unwrap_or_default();

                                    db_handle.db_demote_builder(
                                        demotion.slot,
                                        demotion.builder_pubkey,
                                        demotion.block_hash,
                                        String::from_utf8_lossy(&demotion.reason_msg).into_owned(),
                                        failsafe_triggered.clone(),
                                    );

                                    let token = alert_manager.generate_token(demotion.builder_pubkey);
                                    let message = format_demotion_alert(
                                        demotion.slot,
                                        "",
                                        &operator.name,
                                        &demotion.builder_pubkey,
                                        &builder_id,
                                        &demotion.block_hash,
                                        &String::from_utf8_lossy(&demotion.reason_msg),
                                    );
                                    tracing::debug!(%message, "sending demotion alert");
                                    alert_manager.send_demotion(&message, &token, &builder_id);
                                }
                            }
                            OperatorMessage::Promotion(promotion) => {
                                if local_cache.promote_builder(&promotion.builder_pubkey) {
                                    db_handle.db_promote_builder(promotion.builder_pubkey);

                                    let builder_info = local_cache
                                        .get_builder_info(&promotion.builder_pubkey)
                                        .unwrap_or_default();
                                    alert_manager.send_promotion(
                                        &format!(
                                            "✅ *Optimistic promotion successful*\n*Builder:* `{}`",
                                            promotion.builder_pubkey
                                        ),
                                        builder_info.builder_id(),
                                    );
                                }
                            }
                            OperatorMessage::Collateral(builder_collateral) => {
                                if operator_group == builder_collateral.operator_group {
                                    // ignore collateral messages from our own group
                                    continue;
                                }
                                if operator.operator_group.as_ref().map(|s| s.as_bytes()) != builder_collateral.operator_group.as_ref().map(|v| v.as_slice()) {
                                    tracing::error!(config_operator_group=?operator.operator_group, msg_operator_group=?builder_collateral.operator_group, "operator group mismatch");
                                    continue;
                                }

                                local_cache.update_operator_collateral(
                                    &builder_collateral.builder_pubkey,
                                    &operator.pubkey,
                                    builder_collateral.collateral_wei,
                                    builder_collateral.operator_group,
                                );
                            }
                            OperatorMessage::Payload(payload) => {
                                payload_handler(payload);
                            },
                        }
                    },
                    _ = tokio::time::sleep_until(collateral_resync) => {
                        let ts_ms = utcnow_ms();
                        for (pubkey, info) in local_cache.all_builder_infos_local_collateral_only() {
                            let _ = pubsub.send(OperatorMessage::Collateral(BuilderCollateral { ts_ms, slot: 0, builder_pubkey: pubkey, collateral_wei: info.collateral.to(), operator_group: operator_group.clone() })).await;
                        }
                        collateral_resync = tokio::time::Instant::now() + Duration::from_secs(30);
                    }
                }
            }
        }
    });

    operator_pubsub
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, net::UdpSocket, str::FromStr, time::Duration};

    use alloy_primitives::B256;
    use async_channel::{Receiver, bounded};
    use helix_types::{BlsPublicKeyBytes, Demotion, OperatorMessage, Promotion};
    use libp2p::{Multiaddr, PeerId, identity::Keypair};

    use crate::{Operator, OperatorPubSub, pubsub::TestNetEvent};

    #[tokio::test]
    async fn operator_p2p() {
        let keypair_a = Keypair::generate_secp256k1();
        let keypair_b = Keypair::generate_secp256k1();

        let operator_a = Operator {
            name: "operator A".into(),
            pubkey: keypair_a.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/23032/quic-v1").unwrap(),
            operator_group: None,
        };

        let operator_b = Operator {
            name: "operator B".into(),
            pubkey: keypair_b.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/32023/quic-v1").unwrap(),
            operator_group: None,
        };

        let op_a = OperatorPubSub::new(
            23032,
            keypair_a,
            vec![operator_b],
            helix_common::OperatorP2pMode::On,
        );
        let op_b = OperatorPubSub::new(
            32023,
            keypair_b,
            vec![operator_a],
            helix_common::OperatorP2pMode::On,
        );

        let builder_pubkey = BlsPublicKeyBytes::random();
        let demotion = Demotion {
            ts_ms: 1,
            slot: 1,
            builder_pubkey,
            block_hash: B256::random(),
            reason_msg: "fail".as_bytes().to_vec(),
        };
        let promotion = Promotion { ts_ms: 2, slot: 2, builder_pubkey };
        op_a.send(helix_types::OperatorMessage::Demotion(demotion)).await.unwrap();
        let (_, msg) = op_b.recv().await.unwrap();
        assert!(matches!(msg, OperatorMessage::Demotion(_)));

        op_b.send(OperatorMessage::Promotion(promotion)).await.unwrap();
        let (_, msg) = op_a.recv().await.unwrap();
        assert!(matches!(msg, OperatorMessage::Promotion(_)));
    }

    #[tokio::test]
    async fn replacement_with_same_peer_id_receives_replay_from_incumbents() {
        let mesh = overlapping_replacement_mesh().await;

        assert_eq!(
            receive_demotion_sources(&mesh.replacement, 2).await,
            HashSet::from(["operator A".to_string(), "operator B".to_string()]),
            "replacement should receive each incumbent's subscription-triggered replay",
        );
    }

    #[tokio::test]
    async fn replacement_with_same_peer_id_publishes_after_stale_connection_closes() {
        let mut mesh = overlapping_replacement_mesh().await;
        drop(mesh.original);
        wait_for_connection_closed(&mesh.events_a, mesh.replacement_peer_id, 1).await;
        wait_for_connection_closed(&mesh.events_b, mesh.replacement_peer_id, 1).await;

        let builder_pubkey = BlsPublicKeyBytes::random();
        mesh.replacement
            .send(OperatorMessage::Promotion(Promotion { ts_ms: 3, slot: 3, builder_pubkey }))
            .await
            .unwrap();

        receive_promotion_from(&mut mesh.incumbent_a, "operator C", builder_pubkey).await;
        receive_promotion_from(&mut mesh.incumbent_b, "operator C", builder_pubkey).await;
    }

    struct ReplacementMesh {
        incumbent_a: OperatorPubSub,
        incumbent_b: OperatorPubSub,
        original: OperatorPubSub,
        replacement: OperatorPubSub,
        events_a: Receiver<TestNetEvent>,
        events_b: Receiver<TestNetEvent>,
        replacement_peer_id: PeerId,
    }

    async fn overlapping_replacement_mesh() -> ReplacementMesh {
        let [port_a, port_b, port_c, port_c_replacement] = unused_udp_ports();
        let keypair_a = Keypair::generate_secp256k1();
        let keypair_b = Keypair::generate_secp256k1();
        let keypair_c = Keypair::generate_secp256k1();
        let replacement_peer_id = PeerId::from_public_key(&keypair_c.public());

        let operator_a = operator("operator A", &keypair_a, port_a);
        let operator_b = operator("operator B", &keypair_b, port_b);
        let operator_c = operator("operator C", &keypair_c, port_c);

        // Start C first so A and B each establish exactly one initial connection to it.
        let (original, _) =
            test_pubsub(port_c, keypair_c.clone(), vec![operator_a.clone(), operator_b.clone()]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (incumbent_a, events_a) =
            test_pubsub(port_a, keypair_a, vec![operator_c.clone()]);
        let (incumbent_b, events_b) = test_pubsub(port_b, keypair_b, vec![operator_c]);
        wait_for_connection_established(&events_a, replacement_peer_id, 1).await;
        wait_for_connection_established(&events_b, replacement_peer_id, 1).await;

        incumbent_a
            .send(OperatorMessage::Demotion(Demotion {
                ts_ms: 1,
                slot: 1,
                builder_pubkey: BlsPublicKeyBytes::random(),
                block_hash: B256::random(),
                reason_msg: b"operator A replay".to_vec(),
            }))
            .await
            .unwrap();
        incumbent_b
            .send(OperatorMessage::Demotion(Demotion {
                ts_ms: 2,
                slot: 2,
                builder_pubkey: BlsPublicKeyBytes::random(),
                block_hash: B256::random(),
                reason_msg: b"operator B replay".to_vec(),
            }))
            .await
            .unwrap();
        assert_eq!(
            receive_demotion_sources(&original, 2).await,
            HashSet::from(["operator A".to_string(), "operator B".to_string()]),
        );

        // A replacement process uses C's identity while C's old connections remain alive.
        let (replacement, _) =
            test_pubsub(port_c_replacement, keypair_c, vec![operator_a, operator_b]);
        wait_for_connection_established(&events_a, replacement_peer_id, 2).await;
        wait_for_connection_established(&events_b, replacement_peer_id, 2).await;

        ReplacementMesh {
            incumbent_a,
            incumbent_b,
            original,
            replacement,
            events_a,
            events_b,
            replacement_peer_id,
        }
    }

    fn test_pubsub(
        port: u16,
        keypair: Keypair,
        operators: Vec<Operator>,
    ) -> (OperatorPubSub, Receiver<TestNetEvent>) {
        let (outgoing_msgs, out_recv) = bounded(128);
        let (in_send, incoming_msgs) = bounded(128);
        let (event_send, event_recv) = bounded(128);
        let handle = tokio::spawn(crate::pubsub::run_operator_connection(
            port,
            keypair,
            operators,
            out_recv,
            in_send,
            helix_common::OperatorP2pMode::On,
            Some(event_send),
        ));
        (
            OperatorPubSub { outgoing_msgs, incoming_msgs, task_handle: handle.abort_handle() },
            event_recv,
        )
    }

    fn operator(name: &str, keypair: &Keypair, port: u16) -> Operator {
        Operator {
            name: name.into(),
            pubkey: keypair.public(),
            multiaddr: Multiaddr::from_str(&format!("/ip4/127.0.0.1/udp/{port}/quic-v1")).unwrap(),
            operator_group: None,
        }
    }

    fn unused_udp_ports() -> [u16; 4] {
        let sockets =
            std::array::from_fn::<_, 4, _>(|_| UdpSocket::bind(("127.0.0.1", 0)).unwrap());
        sockets.map(|socket| socket.local_addr().unwrap().port())
    }

    async fn wait_for_connection_established(
        events: &Receiver<TestNetEvent>,
        expected_peer: PeerId,
        expected_count: u32,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let TestNetEvent::ConnectionEstablished { peer_id, num_established } =
                    events.recv().await.unwrap()
                    && peer_id == expected_peer
                    && num_established == expected_count
                {
                    return;
                }
            }
        })
        .await
        .expect("expected peer connection was not established");
    }

    async fn wait_for_connection_closed(
        events: &Receiver<TestNetEvent>,
        expected_peer: PeerId,
        expected_remaining: u32,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let TestNetEvent::ConnectionClosed { peer_id, remaining_established } =
                    events.recv().await.unwrap()
                    && peer_id == expected_peer
                    && remaining_established == expected_remaining
                {
                    return;
                }
            }
        })
        .await
        .expect("stale peer connection was not closed");
    }

    async fn receive_demotion_sources(pubsub: &OperatorPubSub, expected: usize) -> HashSet<String> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut sources = HashSet::new();
            while sources.len() < expected {
                let (operator, message) = pubsub.recv().await.unwrap();
                if matches!(message, OperatorMessage::Demotion(_)) {
                    sources.insert(operator.name);
                }
            }
            sources
        })
        .await
        .expect("timed out waiting for incumbent replay")
    }

    async fn receive_promotion_from(
        pubsub: &mut OperatorPubSub,
        expected_operator: &str,
        expected_pubkey: BlsPublicKeyBytes,
    ) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (operator, message) = pubsub.recv().await.unwrap();
                if operator.name == expected_operator
                    && matches!(
                        message,
                        OperatorMessage::Promotion(Promotion { builder_pubkey, .. })
                            if builder_pubkey == expected_pubkey
                    )
                {
                    return;
                }
            }
        })
        .await
        .expect("incumbent did not receive replacement's live publication");
    }
}
