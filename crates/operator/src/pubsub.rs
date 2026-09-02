use std::{collections::HashMap, time::Duration};

use async_channel::{Receiver, Sender};
use helix_common::OperatorP2pMode;
use helix_types::{BuilderCollateral, OperatorMessage};
use libp2p::{
    PeerId, SwarmBuilder,
    allow_block_list::{self, AllowedPeers},
    futures::StreamExt,
    gossipsub::{self, Event, IdentTopic, MessageAcceptance, MessageAuthenticity, ValidationMode},
    identity::Keypair,
    ping,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use ssz::{Decode, Encode};

use super::{Operator, OperatorError};
use crate::utils::{PromotionState, PromotionStates};

const MAX_OPERATOR_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(NetworkBehaviour)]
struct NetBehaviour {
    allow_list: allow_block_list::Behaviour<AllowedPeers>,
    gossipsub: gossipsub::Behaviour,
    ping: ping::Behaviour,
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
        .max_transmit_size(MAX_OPERATOR_MESSAGE_SIZE)
        .build()
}

fn record_builder_collateral(
    builder_collateral: &mut HashMap<String, BuilderCollateral>,
    builder_id: String,
    collateral: BuilderCollateral,
) -> bool {
    let changed = builder_collateral.get(&builder_id).is_none_or(|existing| {
        collateral.collateral_wei != existing.collateral_wei ||
            existing.builder_pubkeys != collateral.builder_pubkeys
    });
    if changed {
        builder_collateral.insert(builder_id, collateral);
    }
    changed
}

pub(super) async fn run_operator_connection(
    quic_port: u16,
    keypair: Keypair,
    operators: Vec<Operator>,
    outgoing: Receiver<(Option<String>, OperatorMessage)>,
    incoming: Sender<(Operator, OperatorMessage)>,
    mode: OperatorP2pMode,
) -> Result<(), OperatorError> {
    let operator_topic = IdentTopic::new("operator");
    let gossipsub_config = operator_gossipsub_config()?;
    let gossipsub =
        gossipsub::Behaviour::new(MessageAuthenticity::Signed(keypair.clone()), gossipsub_config)
            .map_err(OperatorError::GossipsubBehaviourError)?;

    let mut allow_list = allow_block_list::Behaviour::default();
    for op in &operators {
        allow_list.allow_peer(PeerId::from_public_key(&op.pubkey));
    }

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_quic()
        .with_behaviour(|_key| {
            Ok(NetBehaviour { allow_list, gossipsub, ping: ping::Behaviour::default() })
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
        .collect::<HashMap<_, _>>();

    for (peer_id, operator) in &peers {
        // Dial other operators.
        swarm.behaviour_mut().gossipsub.add_explicit_peer(peer_id);
        if let Err(e) = swarm.dial(operator.multiaddr.clone()) {
            tracing::warn!(?operator, ?e, "failed to dial operator");
        }
    }

    // Demotions keyed by builder pubkey. Sent when a new operator subscribes.
    let mut demotions = PromotionStates::default();
    // Local collateral keyed by builder pubkey. Sent when a new operator subscribes.
    let mut builder_collateral = HashMap::<String, BuilderCollateral>::new();
    // Number of connected peers
    let mut connected_peers = 0u32;

    loop {
        tokio::select! {
            to_send = outgoing.recv() => match to_send {
                Ok((builder_id, msg)) => {
                    let transmit = match &msg {
                        OperatorMessage::Demotion(demotion) => {
                            demotions.demoted(demotion.clone())
                        }
                        OperatorMessage::Promotion(promotion) => {
                            demotions.promoted(promotion.clone())
                        }
                        OperatorMessage::Collateral(collateral) => {
                            let Some(id) = builder_id else {
                                continue;
                            };

                            record_builder_collateral(&mut builder_collateral, id, collateral.clone())
                        }
                        _ => true,
                    };
                    if transmit && connected_peers > 0 {
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

                                    let forward = match &operator_msg {
                                        OperatorMessage::Demotion(demotion) => {
                                            demotions.demoted(demotion.clone())
                                        }
                                        OperatorMessage::Promotion(promotion) => {
                                            demotions.promoted(promotion.clone())
                                        },
                                        _ => true,
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
                                    if forward && matches!(mode, OperatorP2pMode::On) {
                                        let _ = incoming.send((operator.clone(), operator_msg)).await;
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
                                // Send current demotion and collateral state.
                                for state in demotions.iter() {
                                    let msg = match state {
                                        PromotionState::Demoted(demotion) => {
                                            OperatorMessage::Demotion(demotion.clone()).as_ssz_bytes()
                                        }
                                        PromotionState::Promoted(promotion) => {
                                            OperatorMessage::Promotion(promotion.clone()).as_ssz_bytes()
                                        }
                                    };
                                    publish_operator_message(
                                        &mut swarm.behaviour_mut().gossipsub,
                                        &operator_topic,
                                        msg,
                                    );
                                }
                                for (_, collateral) in &builder_collateral {
                                    publish_operator_message(
                                        &mut swarm.behaviour_mut().gossipsub,
                                        &operator_topic,
                                        OperatorMessage::Collateral(collateral.clone()).as_ssz_bytes(),
                                    );
                                }
                            } else {
                                let _ = swarm.disconnect_peer_id(peer_id);
                            }
                        }
                        _ => {}
                    }
                    NetBehaviourEvent::Ping(_) => {}
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    if peers.contains_key(&peer_id) {
                        connected_peers += 1;
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    if peers.contains_key(&peer_id) {
                        connected_peers = connected_peers.saturating_sub(1);
                    }
                }
                _ => {},
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use helix_types::BlsPublicKeyBytes;

    use super::*;

    #[test]
    fn gossipsub_is_configured_for_direct_16_mib_messages() {
        let config = operator_gossipsub_config().unwrap();

        assert_eq!(config.max_transmit_size(), MAX_OPERATOR_MESSAGE_SIZE);
        assert!(config.flood_publish());
        assert!(config.validate_messages());
        assert!(matches!(config.validation_mode(), ValidationMode::Strict));
    }

    #[test]
    fn first_builder_collateral_message_is_recorded_for_publish_and_replay() {
        let mut state = HashMap::new();
        let collateral = BuilderCollateral {
            ts_ms: 1,
            slot: 2,
            builder_pubkeys: vec![BlsPublicKeyBytes::random()],
            collateral_wei: 3,
            operator_group: Some(b"operator-a".to_vec()),
        };

        assert!(
            record_builder_collateral(&mut state, "builder-a".to_string(), collateral.clone(),)
        );
        assert_eq!(state.len(), 1);
        assert!(!record_builder_collateral(&mut state, "builder-a".to_string(), collateral,));
    }
}
