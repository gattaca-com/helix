use std::{sync::Arc, time::Duration};

use async_channel::{Receiver, Sender};
use helix_common::{
    OperatorP2pMode, PromotionMode, local_cache::LocalCache, metrics::OperatorMetrics,
};
use helix_types::OperatorMessage;
use libp2p::{
    PeerId, SwarmBuilder,
    allow_block_list::{self, AllowedPeers},
    connection_limits::{self, ConnectionLimits},
    futures::StreamExt,
    gossipsub::{self, Event, IdentTopic, MessageAcceptance, MessageAuthenticity, ValidationMode},
    identity::Keypair,
    ping,
    swarm::{
        NetworkBehaviour, SwarmEvent,
        dial_opts::{DialOpts, PeerCondition},
    },
};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::{Decode, Encode};
use tokio::time::Instant;

use super::{Operator, OperatorError};
use crate::{
    PayloadCache,
    pool::{PoolError, PoolState},
};

const MAX_OPERATOR_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
/// gossipsub checks `max_transmit_size` against the payload on publish, but against the whole
/// length-prefixed RPC frame on decode. Slack covers signature, pubkey, seqno and topic; without
/// it a payload near the cap is silently rejected by the receiver's codec, which tears down the
/// inbound substream.
const OPERATOR_RPC_OVERHEAD: usize = 1024;
/// QUIC receive windows. gossipsub multiplexes every RPC for a peer onto a single substream, so
/// the per-stream window bounds in-flight payload bytes. The libp2p defaults (10MB stream, 15MB
/// connection) stall multi-MiB payloads at one window per RTT.
const QUIC_STREAM_RECV_WINDOW: u32 = 64 * 1024 * 1024;
const QUIC_CONN_RECV_WINDOW: u32 = 192 * 1024 * 1024;
const QUIC_IDLE_TIMEOUT: u32 = 6_000;
const QUIC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const QUIC_REDIAL_INTERVAL: Duration = Duration::from_secs(10);
const POOL_OBSERVE_INTERVAL: Duration = Duration::from_secs(60);
/// A queued publish is dropped, silently, once this elapses. The libp2p default of 5s is shorter
/// than the time a burst of payloads needs on a congested link.
const PUBLISH_QUEUE_DURATION: Duration = Duration::from_secs(12);

#[derive(NetworkBehaviour)]
struct NetBehaviour {
    allow_list: allow_block_list::Behaviour<AllowedPeers>,
    gossipsub: gossipsub::Behaviour,
    ping: ping::Behaviour,
    limit: connection_limits::Behaviour,
}

/// Gauges carry ETH, not wei: f64 is exact only to ~2^53, far below a wei-denominated bid.
fn wei_to_eth(wei: alloy_primitives::U256) -> f64 {
    wei.saturating_to::<u128>() as f64 / 1e18
}

fn publish_operator_message(
    behaviour: &mut gossipsub::Behaviour,
    topic: &IdentTopic,
    data: Vec<u8>,
) {
    let message_size = data.len();
    if let Err(error) = behaviour.publish(topic.clone(), data) {
        tracing::warn!(?error, message_size, "failed to publish operator message");
    }
}

fn operator_gossipsub_config() -> Result<gossipsub::Config, gossipsub::ConfigBuilderError> {
    gossipsub::ConfigBuilder::default()
        .flood_publish(true)
        .validate_messages()
        .validation_mode(ValidationMode::Strict)
        .max_transmit_size(MAX_OPERATOR_MESSAGE_SIZE + OPERATOR_RPC_OVERHEAD)
        .publish_queue_duration(PUBLISH_QUEUE_DURATION)
        // Every message is Ignored after local delivery, so nothing is ever forwarded and
        // IDONTWANT is pure overhead on the payload messages.
        .idontwant_message_size_threshold(usize::MAX)
        .build()
}

#[allow(clippy::result_large_err, clippy::too_many_arguments)]
pub(super) async fn run_operator_connection(
    quic_port: u16,
    keypair: Keypair,
    operators: Vec<Operator>,
    outgoing: Receiver<OperatorMessage>,
    incoming: Sender<(Operator, OperatorMessage)>,
    mode: OperatorP2pMode,
    operator_group: Option<Vec<u8>>,
    promotion_mode: PromotionMode,
    local_cache: Arc<LocalCache>,
) -> Result<(), OperatorError> {
    let local_group = operator_group.unwrap_or_default();
    let operator_topic = IdentTopic::new("operator");
    let gossipsub_config = operator_gossipsub_config()?;
    let gossipsub =
        gossipsub::Behaviour::new(MessageAuthenticity::Signed(keypair.clone()), gossipsub_config)
            .map_err(OperatorError::GossipsubBehaviourError)?;

    let mut allow_list = allow_block_list::Behaviour::default();
    for op in &operators {
        allow_list.allow_peer(PeerId::from_public_key(&op.pubkey));
    }
    let limit = connection_limits::Behaviour::new(
        ConnectionLimits::default().with_max_established_per_peer(Some(1)),
    );

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_quic_config(|mut cfg| {
            cfg.max_stream_data = QUIC_STREAM_RECV_WINDOW;
            cfg.max_connection_data = QUIC_CONN_RECV_WINDOW;
            cfg.max_idle_timeout = QUIC_IDLE_TIMEOUT;
            cfg.keep_alive_interval = QUIC_KEEPALIVE_INTERVAL;
            cfg
        })
        .with_behaviour(|_key| {
            Ok(NetBehaviour { allow_list, gossipsub, ping: ping::Behaviour::default(), limit })
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();

    // Subscribe to operator topic.
    swarm.behaviour_mut().gossipsub.subscribe(&operator_topic)?;

    // Listen for incoming connections.
    swarm.listen_on(format!("/ip4/0.0.0.0/udp/{quic_port}/quic-v1").parse()?)?;

    // Peers by id.
    let peers = operators
        .into_iter()
        .map(|o| (PeerId::from_public_key(&o.pubkey), o))
        .collect::<FxHashMap<_, _>>();

    for (peer_id, operator) in &peers {
        // Dial other operators.
        swarm.behaviour_mut().gossipsub.add_explicit_peer(peer_id);
        if let Err(e) = swarm.dial(operator.multiaddr.clone()) {
            tracing::warn!(?operator, ?e, "failed to dial operator");
        }
    }

    // Pool state: membership, collateral, promotions and retained reports. Replayed when a new
    // operator subscribes.
    let mut pool = PoolState::new(local_group.clone(), promotion_mode);
    // Number of connected peers
    let mut connected_peers = FxHashSet::default();
    let mut redial_deadline = Instant::now() + QUIC_REDIAL_INTERVAL;
    let mut observe_deadline = Instant::now() + POOL_OBSERVE_INTERVAL;

    // Payload deduplication
    let mut payload_cache = PayloadCache::default();

    loop {
        tokio::select! {
            to_send = outgoing.recv() => match to_send {
                Ok(msg) => {
                    // An error here is a local bug: we published a message that contradicts our
                    // own state.
                    let transmit = match &msg {
                        OperatorMessage::Payload(payload) => Ok(payload_cache.insert(payload)),
                        OperatorMessage::Demotion(d) => pool.apply_demotion(d),
                        OperatorMessage::Promotion(p) => pool.apply_promotion(p),
                        OperatorMessage::Collateral(c) => pool.apply_collateral(local_group.clone(), c),
                        OperatorMessage::Membership(m) => pool.apply_membership(m),
                    };
                    let transmit = match transmit {
                        Ok(changed) => changed,
                        Err(e) => {
                            tracing::error!(%e, "local operator message rejected by pool state");
                            false
                        }
                    };
                    if transmit && promotion_mode.applies() {
                        local_cache.update_pool_state(pool.pool_records(), pool.key_records());
                    }
                    if transmit && !connected_peers.is_empty() {
                        publish_operator_message(
                            &mut swarm.behaviour_mut().gossipsub,
                            &operator_topic,
                            msg.as_ssz_bytes(),
                        );
                    }
                }
                Err(_) => break, // channel closed
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(b_event) => match b_event {
                    NetBehaviourEvent::Gossipsub(g_event) => match g_event {
                        Event::Message { propagation_source, message_id, message } => {
                            // Operator messages are pushed directly to every subscribed peer. Mark
                            // them as ignored by gossipsub after local delivery so they are never
                            // forwarded to another peer.
                            let _ = swarm.behaviour_mut().gossipsub.report_message_validation_result(
                                &message_id,
                                &propagation_source,
                                MessageAcceptance::Ignore,
                            );

                            let Some(source) = message.source else {
                                tracing::warn!(?propagation_source, "received operator message without a source");
                                let _ = swarm.disconnect_peer_id(propagation_source);
                                continue;
                            };

                            match peers.get(&source) {
                                Some(operator) => {
                                    let operator_msg = match OperatorMessage::from_ssz_bytes(&message.data) {
                                        Ok(msg) => msg,
                                        Err(e) => {
                                            tracing::error!(?e, operator=operator.name, "failed to decode operator message");
                                            continue;
                                        }
                                    };

                                    // Resolve the collateral group from the message, else from the
                                    // configured source peer. A missing or conflicting group is
                                    // rejected.
                                    let group = match &operator_msg {
                                        OperatorMessage::Collateral(c) => {
                                            let configured = operator.operator_group.as_ref().map(|g| g.as_bytes());
                                            match (c.operator_group.as_deref(), configured) {
                                                (Some(msg_group), Some(cfg)) if msg_group != cfg => {
                                                    tracing::error!(?cfg, ?msg_group, operator = operator.name, "operator group mismatch");
                                                    continue;
                                                }
                                                (Some(msg_group), _) => msg_group.to_vec(),
                                                (None, Some(cfg)) => cfg.to_vec(),
                                                (None, None) => {
                                                    tracing::error!(operator = operator.name, "collateral message with no resolvable operator group");
                                                    continue;
                                                }
                                            }
                                        }
                                        _ => Vec::new(),
                                    };

                                    let forward = match &operator_msg {
                                        OperatorMessage::Payload(_) => Ok(true),
                                        OperatorMessage::Demotion(d) => pool.apply_demotion(d),
                                        OperatorMessage::Promotion(p) => pool.apply_promotion(p),
                                        OperatorMessage::Collateral(c) => pool.apply_collateral(group, c),
                                        OperatorMessage::Membership(m) => pool.apply_membership(m),
                                    };
                                    let forward = match forward {
                                        Ok(changed) => changed,
                                        Err(PoolError::Capacity) => {
                                            tracing::warn!(operator = operator.name, "pool state at capacity, message dropped");
                                            false
                                        }
                                        Err(e) => {
                                            tracing::error!(%e, operator = operator.name, "operator message rejected by pool state");
                                            false
                                        }
                                    };

                                    match &operator_msg {
                                        OperatorMessage::Payload(p) => tracing::info!(
                                            slot = p.slot,
                                            block_hash = %p.execution_payload.execution_payload.block_hash,
                                            operator = operator.name,
                                            "new operator payload"
                                        ),
                                        _ => tracing::info!(?operator_msg, operator = operator.name, "new operator message"),
                                    }

                                    if forward && promotion_mode.applies() {
                                        local_cache
                                            .update_pool_state(pool.pool_records(), pool.key_records());
                                    }

                                    // Always handle payload messages
                                    if forward && (matches!(mode, OperatorP2pMode::On) || matches!(operator_msg, OperatorMessage::Payload(_)))
                                        && let Err(e) = incoming.try_send((operator.clone(), operator_msg))
                                    {
                                        tracing::warn!(?e, "failed to forward operator message");
                                    }
                                }
                                None => {
                                    tracing::warn!(?source, ?propagation_source, "received operator message from unknown peer");
                                    let _ = swarm.disconnect_peer_id(propagation_source);
                                }
                            }
                        }
                        Event::Subscribed { peer_id, topic } => {
                            if peers.contains_key(&peer_id) && topic == operator_topic.hash() {
                                // Membership, local collateral, promotions, then retained reports.
                                for msg in pool.replay() {
                                    publish_operator_message(
                                        &mut swarm.behaviour_mut().gossipsub,
                                        &operator_topic,
                                        msg.as_ssz_bytes(),
                                    );
                                }
                            } else {
                                let _ = swarm.disconnect_peer_id(peer_id);
                            }
                        }
                        Event::SlowPeer { peer_id, failed_messages } => {
                            // Queue timeouts and full queues are otherwise invisible; large
                            // payload messages are the first to be dropped.
                            tracing::warn!(
                                operator = peers.get(&peer_id).map_or("unknown", |o| o.name.as_str()),
                                ?failed_messages,
                                "operator peer dropped queued gossip messages"
                            );
                        }
                        _ => {}
                    }
                    NetBehaviourEvent::Ping(_) => {}
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    if peers.contains_key(&peer_id) {
                        connected_peers.insert(peer_id);
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    connected_peers.remove(&peer_id);
                }
                _ => {},
            },
            _ = tokio::time::sleep_until(observe_deadline) => {
                observe_deadline += POOL_OBSERVE_INTERVAL;
                for pool in pool.observe() {
                    let id = String::from_utf8_lossy(pool.collateral_id);
                    OperatorMetrics::collateral_pool(&id, "gross", wei_to_eth(pool.gross));
                    OperatorMetrics::collateral_pool(&id, "reserved", wei_to_eth(pool.reserved));
                    OperatorMetrics::collateral_pool(&id, "available", wei_to_eth(pool.available));
                    OperatorMetrics::collateral_pool(&id, "reports", pool.reports as f64);
                    OperatorMetrics::collateral_pool(&id, "members", pool.members as f64);
                    OperatorMetrics::collateral_pool(&id, "optimistic", pool.optimistic as f64);
                }
            },
            _ = tokio::time::sleep_until(redial_deadline) => {
                redial_deadline += QUIC_REDIAL_INTERVAL;
                for (peer, operator) in &peers {
                    if !connected_peers.contains(peer)
                        && let Err(e) = swarm.dial( DialOpts::peer_id(*peer)
                            .addresses(vec![operator.multiaddr.clone()])
                            .condition(PeerCondition::DisconnectedAndNotDialing)
                            .build()
                        )
                    {
                        tracing::warn!(?operator, ?e, "failed to redial operator");
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossipsub_is_configured_for_direct_16_mib_messages() {
        let config = operator_gossipsub_config().unwrap();

        assert_eq!(config.max_transmit_size(), MAX_OPERATOR_MESSAGE_SIZE + OPERATOR_RPC_OVERHEAD);
        assert_eq!(config.publish_queue_duration(), PUBLISH_QUEUE_DURATION);
        assert!(config.flood_publish());
        assert!(config.validate_messages());
        assert!(matches!(config.validation_mode(), ValidationMode::Strict));
    }
}
