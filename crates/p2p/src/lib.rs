use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::WebSocketUpgrade,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    Extension,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PPeerConfig};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, Transaction};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

use crate::{
    il_consensus::{compute_final_inclusion_list, compute_shared_inclusion_list},
    messages::{HelloMessage, InclusionListMessage, P2PMessage, SignedP2PMessage},
};

pub(crate) mod il_consensus;
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
        // Upgrade connection to WebSocket, spawning a new task to handle the connection
        let ws = ws
            .on_failed_upgrade(|error| warn!(%error, "websocket upgrade failed"))
            .on_upgrade(|socket| async move { api.handle_ws_connection(socket, None).await });

        Ok(ws)
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
                    let il: Vec<_> = inclusion_list.txs.into_iter().map(Transaction).collect();
                    let shared_il = compute_shared_inclusion_list(&vote_map, slot, il.into());

                    let txs = shared_il.into_iter().map(|tx| tx.to_vec().into()).collect();
                    let inclusion_list = InclusionList { txs };

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
                    let vote_map = std::mem::take(&mut vote_map);
                    let il: Vec<_> = inclusion_list.txs.into_iter().map(Transaction).collect();
                    let inclusion_list = compute_final_inclusion_list(vote_map, slot, il.into());
                    let txs: Vec<_> =
                        inclusion_list.into_iter().map(|tx| tx.to_vec().into()).collect();
                    let _ = result_tx
                        .send(Some(InclusionList { txs }))
                        .inspect_err(|e| error!(err=?e, "failed to send settled inclusion list"));
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
