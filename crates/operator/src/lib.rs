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
    OperatorConfig, OperatorP2pMode, PromotionMode,
    alerts::{AlertManager, format_demotion_alert},
    local_cache::LocalCache,
    utils::utcnow_ms,
};
use helix_database::{PostgresDatabaseService, handle::DbHandle};
use helix_types::{BuilderCollateral, CollateralMembership, Operator, OperatorMessage, Payload};
use libp2p::{BehaviourBuilderError, TransportError, gossipsub, identity::Keypair, multiaddr};
use thiserror::Error;
use tokio::task::AbortHandle;

mod payload;
mod pool;
mod pubsub;
mod utils;

pub use libp2p::identity::Keypair as OperatorKeypair;
pub use utils::keypair_from_bytes;

use crate::{payload::PayloadCache, utils::load_operator_keypair};

#[derive(Debug, Error)]
pub enum OperatorError {
    MultiaddrParseError(#[from] multiaddr::Error),
    SwarmNetworkError(#[from] TransportError<std::io::Error>),
    SwarmBuildError(#[from] BehaviourBuilderError),
    GossipsubConfigError(#[from] gossipsub::ConfigBuilderError),
    GossipsubBehaviourError(&'static str),
    GossipsubSubscriptionError(#[from] gossipsub::SubscriptionError),
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

#[allow(clippy::result_large_err)]
impl OperatorPubSub {
    pub fn new(
        quic_port: u16,
        local_keypair: Keypair,
        operators: Vec<Operator>,
        mode: OperatorP2pMode,
        operator_group: Option<Vec<u8>>,
        promotion_mode: PromotionMode,
        local_cache: Arc<LocalCache>,
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
            operator_group,
            promotion_mode,
            local_cache,
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

#[allow(clippy::too_many_arguments)]
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
    let promotion_mode = config.promotion_mode;
    local_cache.set_promotion_mode(promotion_mode);
    let operator_pubsub = Arc::new(OperatorPubSub::new(
        config.quic_port,
        operator_keypair,
        config.operators,
        config.mode,
        operator_group.clone(),
        promotion_mode,
        local_cache.clone(),
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
            // Spec replay order: membership, local collateral, promotions, retained reports.
            let now = utcnow_ms();
            for (builder_id, (builder_pubkeys, collateral)) in cache.all_builder_local_collateral()
            {
                let collateral_id = builder_id.into_bytes();
                let _ = pubsub
                    .send(OperatorMessage::Membership(CollateralMembership {
                        ts_ms: now,
                        collateral_id: collateral_id.clone(),
                        builder_pubkeys,
                    }))
                    .await;
                let _ = pubsub
                    .send(OperatorMessage::Collateral(BuilderCollateral {
                        ts_ms: now,
                        slot: 0,
                        collateral_id,
                        collateral_wei: collateral.to(),
                        operator_group: group.clone(),
                    }))
                    .await;
            }

            // Promotions must precede reports: retention compares against the promotion timestamp.
            match db_service.load_promotions().await {
                Ok(promotions) => {
                    for promotion in promotions {
                        let _ = pubsub.send(OperatorMessage::Promotion(promotion)).await;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        ?e,
                        "failed to load promotions from DB. Operators not updated."
                    );
                }
            }

            match db_service.load_retained_demotions().await {
                Ok(demotions) => {
                    for demotion in demotions {
                        let _ = pubsub.send(OperatorMessage::Demotion(demotion)).await;
                    }
                }
                Err(e) => {
                    tracing::error!(?e, "failed to load demotions from DB. Operators not updated.");
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
                                let builder_id = local_cache
                                    .get_builder_info(&demotion.builder_pubkey)
                                    .and_then(|info| info.builder_id)
                                    .unwrap_or_default();
                                let newly_demoted =
                                    local_cache.demote_builder(&demotion.builder_pubkey);

                                // Persist every distinct report, including one for a key that is
                                // already demoted: a later promotion may supersede only some of
                                // them, and each contributes to its own slot's reservation.
                                // Duplicates are already filtered by pool state before forwarding.
                                db_handle.db_demote_builder(
                                    demotion.slot,
                                    demotion.builder_pubkey,
                                    demotion.block_hash,
                                    String::from_utf8_lossy(&demotion.collateral_id).into_owned(),
                                    U256::from(demotion.bid_value_wei),
                                    String::from_utf8_lossy(&demotion.reason_msg).into_owned(),
                                    failsafe_triggered.clone(),
                                );

                                if newly_demoted {
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
                                let collateral_id =
                                    String::from_utf8_lossy(&promotion.collateral_id).into_owned();
                                // The promotion record is the retention watermark, so persist it
                                // whether or not a local flag changed. The upsert never moves the
                                // watermark backwards.
                                db_handle.db_promote_builder(
                                    promotion.builder_pubkey,
                                    collateral_id,
                                    promotion.ts_ms,
                                    promotion.slot,
                                );

                                // The flag stays per pubkey. The pool-wide effect lives in pool
                                // state, which admission reads from `Follow`.
                                let promoted =
                                    local_cache.promote_builder(&promotion.builder_pubkey);

                                if promoted {
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
                            // Collateral and membership are held in pool state in the pubsub
                            // task, keyed by (operator group, pool). Nothing on the live auction
                            // path reads them below `Share`.
                            OperatorMessage::Collateral(_) | OperatorMessage::Membership(_) => {}
                            OperatorMessage::Payload(payload) => {
                                payload_handler(payload);
                            },
                        }
                    },
                    _ = tokio::time::sleep_until(collateral_resync) => {
                        let ts_ms = utcnow_ms();
                        for (builder_id, (builder_pubkeys, collateral)) in local_cache.all_builder_local_collateral() {
                            let collateral_id = builder_id.into_bytes();
                            let _ = pubsub.send(OperatorMessage::Membership(CollateralMembership {
                                ts_ms,
                                collateral_id: collateral_id.clone(),
                                builder_pubkeys,
                            })).await;
                            let _ = pubsub.send(OperatorMessage::Collateral(BuilderCollateral {
                                ts_ms,
                                slot: 0,
                                collateral_id,
                                collateral_wei: collateral.to(),
                                operator_group: operator_group.clone(),
                            })).await;
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
    use std::{str::FromStr, sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use helix_common::local_cache::LocalCache;
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
            None,
            helix_common::PromotionMode::Observe,
            Arc::new(LocalCache::new_test()),
        );
        // Ensure A is listening before B initiates its dial.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let op_b = OperatorPubSub::new(
            32023,
            keypair_b,
            vec![operator_a],
            helix_common::OperatorP2pMode::On,
            None,
            helix_common::PromotionMode::Observe,
            Arc::new(LocalCache::new_test()),
        );
        // Wait for the gossipsub subscription exchange before publishing. Messages are
        // intentionally best-effort and are not queued for peers that have not subscribed yet.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let builder_pubkey = BlsPublicKeyBytes::random();
        let demotion = Demotion {
            ts_ms: 1,
            slot: 1,
            collateral_id: b"C1".to_vec(),
            builder_pubkey,
            block_hash: B256::random(),
            bid_value_wei: 1,
            // Exercise a message larger than floodsub's former 2 KiB frame limit.
            reason_msg: vec![42; 4 * 1024],
        };
        let promotion =
            Promotion { ts_ms: 2, slot: 2, collateral_id: b"C1".to_vec(), builder_pubkey };
        op_a.send(helix_types::OperatorMessage::Demotion(demotion)).await.unwrap();
        let (_, msg) = tokio::time::timeout(Duration::from_secs(5), op_b.recv())
            .await
            .expect("timed out waiting for demotion")
            .unwrap();
        assert!(
            matches!(msg, OperatorMessage::Demotion(demotion) if demotion.reason_msg.len() == 4 * 1024)
        );

        op_b.send(OperatorMessage::Promotion(promotion)).await.unwrap();
        let (_, msg) = tokio::time::timeout(Duration::from_secs(5), op_a.recv())
            .await
            .expect("timed out waiting for promotion")
            .unwrap();
        assert!(matches!(msg, OperatorMessage::Promotion(_)));
    }
}
