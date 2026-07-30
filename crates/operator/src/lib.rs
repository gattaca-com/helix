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
    OperatorConfig,
    alerts::{AlertManager, format_demotion_alert},
    local_cache::LocalCache,
    utils::utcnow_ms,
};
use helix_database::{PostgresDatabaseService, handle::DbHandle};
use helix_types::{BuilderCollateral, Operator, OperatorMessage};
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
    pub fn new(quic_port: u16, local_keypair: Keypair, operators: Vec<Operator>) -> Self {
        let (outgoing_msgs, out_recv) = bounded(128);
        let (in_send, incoming_msgs) = bounded(128);

        // p2p task.
        let handle = tokio::spawn(pubsub::run_operator_connection(
            quic_port,
            local_keypair,
            operators,
            out_recv,
            in_send,
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

pub fn spawn_operator_connection(
    config: OperatorConfig,
    loaded: Arc<AtomicBool>,
    local_cache: Arc<LocalCache>,
    db_handle: DbHandle,
    db_service: Arc<PostgresDatabaseService>,
    failsafe_triggered: Arc<AtomicBool>,
    alert_manager: Arc<AlertManager>,
) -> Arc<OperatorPubSub> {
    // if there is `OperatorConfig`, then operator key is expected.
    let operator_keypair = load_operator_keypair();
    let operator_pubsub =
        Arc::new(OperatorPubSub::new(config.quic_port, operator_keypair, config.operators));

    // spawn a task to load initial db state
    tokio::spawn({
        let pubsub = operator_pubsub.clone();
        let cache = local_cache.clone();
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
                                local_cache.update_operator_collateral(
                                    &builder_collateral.builder_pubkey,
                                    &operator.pubkey,
                                    builder_collateral.collateral_wei,
                                );
                            }
                        }
                    },
                    _ = tokio::time::sleep_until(collateral_resync) => {
                        let ts_ms = utcnow_ms();
                        for (pubkey, info) in local_cache.all_builder_infos_local_collateral_only() {
                            let _ = pubsub.send(OperatorMessage::Collateral(BuilderCollateral { ts_ms, slot: 0, builder_pubkey: pubkey, collateral_wei: info.collateral.to() })).await;
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
    use std::str::FromStr;

    use alloy_primitives::B256;
    use helix_types::{BlsPublicKeyBytes, Demotion, OperatorMessage, Promotion};
    use libp2p::{Multiaddr, identity::Keypair};

    use crate::{Operator, OperatorPubSub};

    #[tokio::test]
    async fn operator_p2p() {
        let keypair_a = Keypair::generate_secp256k1();
        let keypair_b = Keypair::generate_secp256k1();

        let operator_a = Operator {
            name: "operator A".into(),
            pubkey: keypair_a.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/23032/quic-v1").unwrap(),
        };

        let operator_b = Operator {
            name: "operator B".into(),
            pubkey: keypair_b.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/32023/quic-v1").unwrap(),
        };

        let op_a = OperatorPubSub::new(23032, keypair_a, vec![operator_b]);
        let op_b = OperatorPubSub::new(32023, keypair_b, vec![operator_a]);

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
}
