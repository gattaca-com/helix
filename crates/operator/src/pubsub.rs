use std::{collections::HashMap, time::Duration};

use async_channel::{Receiver, Sender};
use helix_common::OperatorP2pMode;
use helix_types::{BlsPublicKeyBytes, BuilderCollateral, OperatorMessage};
use libp2p::{
    PeerId, SwarmBuilder,
    allow_block_list::{self, AllowedPeers},
    floodsub::{self, Event, FloodsubMessage},
    futures::StreamExt,
    identity::Keypair,
    ping,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use ssz::{Decode, Encode};

use super::{Operator, OperatorError};
use crate::utils::{PromotionState, PromotionStates};

#[derive(NetworkBehaviour)]
struct NetBehaviour {
    allow_list: allow_block_list::Behaviour<AllowedPeers>,
    floodsub: floodsub::Behaviour,
    ping: ping::Behaviour,
}

#[cfg(test)]
#[derive(Debug)]
pub(super) enum TestNetEvent {
    ConnectionEstablished { peer_id: PeerId, num_established: u32 },
    ConnectionClosed { peer_id: PeerId, remaining_established: u32 },
}

pub(super) async fn run_operator_connection(
    quic_port: u16,
    keypair: Keypair,
    operators: Vec<Operator>,
    outgoing: Receiver<OperatorMessage>,
    incoming: Sender<(Operator, OperatorMessage)>,
    mode: OperatorP2pMode,
    #[cfg(test)] test_events: Option<Sender<TestNetEvent>>,
) -> Result<(), OperatorError> {
    let floodsub_topic = floodsub::Topic::new("operator");
    let local_peer_id = PeerId::from_public_key(&keypair.public());

    let mut allow_list = allow_block_list::Behaviour::default();
    for op in &operators {
        allow_list.allow_peer(PeerId::from_public_key(&op.pubkey));
    }

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_quic()
        .with_behaviour(|_key| {
            Ok(NetBehaviour {
                allow_list,
                floodsub: floodsub::Behaviour::new(local_peer_id),
                ping: ping::Behaviour::default(),
            })
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
        .build();

    // Subscribe to operator topic.
    swarm.behaviour_mut().floodsub.subscribe(floodsub_topic.clone());

    // Listen for incoming connections.
    swarm.listen_on(format!("/ip4/0.0.0.0/udp/{quic_port}/quic-v1").parse()?)?;

    // Peers by id.
    let peers = operators
        .into_iter()
        .map(|o| (PeerId::from_public_key(&o.pubkey), o))
        .collect::<HashMap<_, _>>();

    for (peer_id, operator) in &peers {
        // Dial other operators.
        swarm.behaviour_mut().floodsub.add_node_to_partial_view(*peer_id);
        if let Err(e) = swarm.dial(operator.multiaddr.clone()) {
            tracing::warn!(?operator, ?e, "failed to dial operator");
        }
    }

    // Demotions keyed by builder pubkey. Sent when a new operator subscribes.
    let mut demotions = PromotionStates::default();
    // Local collateral keyed by builder pubkey. Sent when a new operator subscribes.
    let mut builder_collateral = HashMap::<BlsPublicKeyBytes, BuilderCollateral>::new();
    // Number of connected peers
    let mut connected_peers = 0u32;

    loop {
        tokio::select! {
            to_send = outgoing.recv() => match to_send {
                Ok(msg) => {
                    let transmit = match &msg {
                        OperatorMessage::Demotion(demotion) => {
                            demotions.demoted(demotion.clone())
                        }
                        OperatorMessage::Promotion(promotion) => {
                            demotions.promoted(promotion.clone())
                        }
                        OperatorMessage::Collateral(collateral) => {
                            let existing = builder_collateral.get(&collateral.builder_pubkey).map(|bc| bc.collateral_wei).unwrap_or_default();
                            if existing != collateral.collateral_wei {
                                builder_collateral.insert(collateral.builder_pubkey, collateral.clone());
                                true
                            } else {
                                false
                            }
                        }
                        _ => true,
                    };
                    if transmit && connected_peers > 0 {
                        swarm.behaviour_mut().floodsub.publish(floodsub_topic.clone(), msg.as_ssz_bytes());
                    }
                }
                Err(_) => break, // channel closed
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(b_event) => match b_event {
                    NetBehaviourEvent::Floodsub(f_event) => match f_event {
                        Event::Message(msg) => {
                            let FloodsubMessage { source, data, .. } = msg;

                            match peers.get(&source) {
                                Some(operator) => {
                                    let operator_msg = match OperatorMessage::from_ssz_bytes(&data) {
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

                                    tracing::info!(?operator_msg, operator=operator.name, "new operator message");
                                    if forward && matches!(mode, OperatorP2pMode::On) {
                                        let _ = incoming.send((operator.clone(), operator_msg)).await;
                                    }
                                }
                                None => {
                                    tracing::warn!(?source, "received operator message from unknown peer");
                                    let _ = swarm.disconnect_peer_id(source);
                                }
                            }
                        }
                        Event::Subscribed { peer_id, topic } => {
                            if peers.contains_key(&peer_id) && topic == floodsub_topic {
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
                                    swarm.behaviour_mut().floodsub.publish(floodsub_topic.clone(), msg);
                                }
                                for (_, collateral) in &builder_collateral {
                                    swarm.behaviour_mut().floodsub.publish(floodsub_topic.clone(), OperatorMessage::Collateral(collateral.clone()).as_ssz_bytes());
                                }
                            } else {
                                let _ = swarm.disconnect_peer_id(peer_id);
                            }
                        }
                        _ => {}
                    }
                    NetBehaviourEvent::Ping(_) => {}
                }
                SwarmEvent::ConnectionEstablished { peer_id, num_established, .. } => {
                    if peers.contains_key(&peer_id) {
                        connected_peers += 1;
                        #[cfg(test)]
                        if let Some(events) = &test_events {
                            let _ = events.try_send(TestNetEvent::ConnectionEstablished {
                                peer_id,
                                num_established: num_established.get(),
                            });
                        }
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                    if peers.contains_key(&peer_id) {
                        connected_peers = connected_peers.saturating_sub(1);
                        #[cfg(test)]
                        if let Some(events) = &test_events {
                            let _ = events.try_send(TestNetEvent::ConnectionClosed {
                                peer_id,
                                remaining_established: num_established,
                            });
                        }
                    }
                }
                _ => {},
            }
        }
    }

    Ok(())
}
