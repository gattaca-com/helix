use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_consensus::TxEnvelope;
use alloy_primitives::Bytes;
use alloy_rlp::Decodable;
use axum::{
    extract::WebSocketUpgrade,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    Extension,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PPeerConfig};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, Transaction, Transactions};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tracing::{error, info};
use tree_hash::TreeHash;

use crate::messages::{HelloMessage, InclusionListMessage, P2PMessage, SignedP2PMessage};

pub mod messages;

enum P2PApiRequest {
    PeerMessage((BlsPublicKeyBytes, InclusionListMessage)),
    LocalInclusionList {
        slot: u64,
        inclusion_list: InclusionList,
        result_tx: oneshot::Sender<Option<InclusionList>>,
    },
    SharedInclusionList {
        slot: u64,
        inclusion_list: InclusionList,
        result_tx: oneshot::Sender<Option<InclusionList>>,
    },
    SettledInclusionList {
        slot: u64,
        inclusion_list: InclusionList,
        result_tx: oneshot::Sender<Option<InclusionList>>,
    },
}

pub struct P2PApi {
    broadcast_tx: broadcast::Sender<SignedP2PMessage>,
    api_requests_tx: mpsc::Sender<P2PApiRequest>,
    signing_context: Arc<RelaySigningContext>,
    peer_configs: Vec<P2PPeerConfig>,
}

impl P2PApi {
    pub fn new(
        peer_configs: Vec<P2PPeerConfig>,
        signing_context: Arc<RelaySigningContext>,
    ) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(100);
        let (api_requests_tx, api_requests_rx) = mpsc::channel(2000);
        let this = Arc::new(Self { peer_configs, broadcast_tx, api_requests_tx, signing_context });
        for peer_config in &this.peer_configs {
            let uri: Uri = peer_config
                .url
                .parse()
                .inspect_err(|e| error!(err=?e, "invalid peer URL"))
                .unwrap();
            let this_clone = this.clone();
            let peer_config = peer_config.clone();
            tokio::spawn(async move {
                let (ws, _response) = connect_async(uri).await.unwrap();
                this_clone.handle_ws_connection(ws, Some(peer_config.verifying_key)).await;
            });
        }
        tokio::spawn(this.clone().handle_incoming_messages(api_requests_rx));
        this
    }

    pub fn broadcast(&self, message: P2PMessage) {
        let signed_message = message.sign(&self.signing_context);
        // Ignore error if there are no active receivers.
        let _ = self.broadcast_tx.send(signed_message);
    }

    pub async fn share_inclusion_list(
        &self,
        slot: u64,
        inclusion_list: InclusionList,
    ) -> Option<InclusionList> {
        let (result_tx, result_rx) = oneshot::channel();
        self.api_requests_tx
            .send(P2PApiRequest::LocalInclusionList { slot, inclusion_list, result_tx })
            .await
            .unwrap();
        result_rx.await.unwrap()
    }

    #[tracing::instrument(skip_all)]
    pub async fn p2p_connect(
        Extension(api): Extension<Arc<P2PApi>>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, P2PApiError> {
        info!("got new peer connection");
        Ok(ws.on_upgrade(|socket| async move { api.handle_ws_connection(socket, None).await }))
    }

    async fn handle_ws_connection<M, S, E>(
        &self,
        mut socket: S,
        mut peer_pubkey: Option<BlsPublicKey>,
    ) where
        S: Stream<Item = Result<M, E>> + Sink<M> + Unpin,
        M: TryFrom<SignedP2PMessage> + std::fmt::Debug,
        SignedP2PMessage: TryFrom<M>,
        <M as TryFrom<SignedP2PMessage>>::Error: std::fmt::Debug,
        <SignedP2PMessage as TryFrom<M>>::Error: std::fmt::Debug,
        <S as futures::Sink<M>>::Error: std::fmt::Debug,
        E: std::fmt::Debug,
    {
        let mut broadcast_rx = self.broadcast_tx.subscribe();
        let hello_message =
            P2PMessage::Hello(HelloMessage { pubkey: self.signing_context.keypair.pk.clone() });
        let signed_hello_message = hello_message.sign(&self.signing_context);
        if let Err(err) = socket.send(signed_hello_message.try_into().unwrap()).await {
            error!(err=?err, "failed to send hello message");
            return;
        }
        loop {
            tokio::select! {
                res = broadcast_rx.recv() => {
                    let Ok(msg) = res else {
                        // Sender was closed
                        break;
                    };
                    let serialized = msg.try_into().unwrap();
                    socket.send(serialized).await.unwrap();
                },
                res = socket.next() => {
                    let Some(res) = res else {
                        // Socket was closed
                        break;
                    };
                    let Ok(msg) = res.inspect_err(|e| error!(err=?e, "failed to receive websocket message")) else {
                        break;
                    };
                    let msg: SignedP2PMessage = msg.try_into().unwrap();
                    // Handle Hello message in-place
                    if let P2PMessage::Hello(hello_msg) = &msg.message {
                        // TODO: check the pubkey is from a known peer
                        msg.verify_signature(&hello_msg.pubkey).unwrap();
                        info!("Verified Hello message from peer: {}", hello_msg.pubkey);

                        if let Some(pubkey) = &peer_pubkey {
                            if pubkey != &hello_msg.pubkey {
                                error!("Received unexpected pubkey. Got {pubkey}, expected {}", hello_msg.pubkey);
                                return;
                            }
                            continue;
                        }
                        peer_pubkey = Some(hello_msg.pubkey.clone());
                        continue;
                    } else if peer_pubkey.is_none() {
                        error!("First message is not a Hello. Disconnecting from peer...");
                        return;
                    }
                    // Verify message signature and send to main process
                    msg.verify_signature(peer_pubkey.as_ref().unwrap()).unwrap();
                    let P2PMessage::InclusionList(il_msg) = msg.message else {
                        unreachable!("already checked")
                    };
                    self.api_requests_tx.send(P2PApiRequest::PeerMessage((peer_pubkey.as_ref().unwrap().serialize().into(), il_msg))).await.unwrap();
                }
            };
        }
    }

    async fn handle_incoming_messages(
        self: Arc<Self>,
        mut api_requests_rx: mpsc::Receiver<P2PApiRequest>,
    ) {
        const CUTOFF_TIME_1: Duration = Duration::from_secs(2);
        const CUTOFF_TIME_2: Duration = Duration::from_secs(2);
        let mut vote_map: HashMap<BlsPublicKeyBytes, (u64, messages::InclusionList)> =
            HashMap::new();
        while let Some(request) = api_requests_rx.recv().await {
            match request {
                P2PApiRequest::LocalInclusionList { slot, inclusion_list, result_tx } => {
                    self.broadcast(InclusionListMessage::new(slot, inclusion_list.clone()).into());
                    let api_requests_tx = self.api_requests_tx.clone();
                    let shared_il_msg =
                        P2PApiRequest::SharedInclusionList { slot, inclusion_list, result_tx };
                    tokio::spawn(async move {
                        // Sleep for t_1 time
                        tokio::time::sleep(CUTOFF_TIME_1).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SharedInclusionList { slot, inclusion_list, result_tx } => {
                    let mut tx_frequency = HashMap::new();
                    vote_map.retain(|_, (il_slot, _)| *il_slot >= slot);
                    for (_peer, (_il_slot, il)) in &vote_map {
                        for tx in il.iter() {
                            let Ok(decoded_tx) = TxEnvelope::decode(&mut &tx[..]) else {
                                continue;
                            };

                            tx_frequency
                                .entry(*decoded_tx.tx_hash())
                                .and_modify(|(_, c)| *c += 1)
                                .or_insert((tx.clone(), 1));
                        }
                    }
                    inclusion_list.txs.iter().for_each(|tx| {
                        let Ok(decoded_tx) = TxEnvelope::decode(&mut &tx[..]) else {
                            return;
                        };

                        tx_frequency
                            .entry(*decoded_tx.tx_hash())
                            .and_modify(|(_, c)| *c += 1)
                            .or_insert((Transaction(tx.clone()), 1));
                    });
                    let mut tx_frequency_ordered: Vec<_> = tx_frequency.into_iter().collect();
                    // Order descending by (frequency, tx_hash)
                    tx_frequency_ordered.sort_unstable_by(
                        |(tx_hash1, (_, freq1)), (tx_hash2, (_, freq2))| {
                            freq2.cmp(freq1).then(tx_hash2.cmp(tx_hash1))
                        },
                    );

                    let mut bytes_available = 8000;
                    let final_il: Vec<_> = tx_frequency_ordered
                        .into_iter()
                        .map(|(_, (tx, _))| Bytes::from(tx.to_vec()))
                        .filter(|tx| {
                            if tx.len() > bytes_available {
                                false
                            } else {
                                bytes_available -= tx.len();
                                true
                            }
                        })
                        .collect();
                    let inclusion_list = InclusionList { txs: final_il };
                    self.broadcast(InclusionListMessage::new(slot, inclusion_list.clone()).into());
                    let api_requests_tx = self.api_requests_tx.clone();
                    let shared_il_msg =
                        P2PApiRequest::SettledInclusionList { slot, inclusion_list, result_tx };
                    tokio::spawn(async move {
                        // Sleep for t_2 time
                        tokio::time::sleep(CUTOFF_TIME_2).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SettledInclusionList { slot, inclusion_list, result_tx } => {
                    let mut il_by_frequency = HashMap::new();
                    let il: Transactions =
                        inclusion_list.txs.into_iter().map(Transaction).collect::<Vec<_>>().into();
                    il_by_frequency.insert(il.tree_hash_root(), (il, 1));
                    vote_map.into_iter().for_each(|(_, (il_slot, il))| {
                        if il_slot != slot {
                            return;
                        }
                        let il_hash = il.tree_hash_root();
                        il_by_frequency
                            .entry(il_hash)
                            .and_modify(|(_, c)| *c += 1)
                            .or_insert((il, 1));
                    });
                    let txs = il_by_frequency
                        .into_iter()
                        .max_by_key(|(il_hash, (il, c))| {
                            (*c, il.iter().map(|tx| tx.len()).sum::<usize>(), *il_hash)
                        })
                        .map(|(_, (il, _))| {
                            il.into_iter().map(|tx| tx.to_vec().into()).collect::<Vec<_>>()
                        });
                    let inclusion_list = txs.map(|txs| InclusionList { txs: txs.into() });
                    let _ = result_tx
                        .send(inclusion_list)
                        .inspect_err(|e| error!(err=?e, "failed to send settled inclusion list"));
                    vote_map = HashMap::new();
                }
                P2PApiRequest::PeerMessage((sender, il_msg)) => {
                    // TODO: validate inclusion lists?
                    vote_map.insert(sender, (il_msg.slot, il_msg.inclusion_list));
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum P2PApiError {
    #[error("internal server error")]
    Foo,
}

impl IntoResponse for P2PApiError {
    fn into_response(self) -> Response {
        let code = match self {
            P2PApiError::Foo => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (code, self.to_string()).into_response()
    }
}
