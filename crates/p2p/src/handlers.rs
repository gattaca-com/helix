use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{extract::WebSocketUpgrade, response::IntoResponse, Extension};
use futures::{SinkExt, StreamExt};
use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PPeerConfig};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use tracing::{error, info, warn};

use crate::{
    il_consensus::{compute_final_inclusion_list, compute_shared_inclusion_list},
    messages::{
        EncodingError, InclusionListMessage, MessageAuthenticationError, P2PMessage, RawP2PMessage,
        SignedHelloMessage,
    },
    socket::PeerSocket,
    P2PApi,
};

#[derive(Debug)]
pub(crate) enum P2PApiRequest {
    PeerMessage { sender: BlsPublicKeyBytes, message: P2PMessage },
    LocalInclusionList(InclusionListRequest),
    SharedInclusionList(InclusionListRequest),
    SettledInclusionList(InclusionListRequest),
}

#[derive(Debug)]
pub(crate) struct InclusionListRequest {
    pub(crate) slot: u64,
    pub(crate) inclusion_list: InclusionList,
    pub(crate) result_tx: oneshot::Sender<Option<InclusionList>>,
}

impl P2PApi {
    /// Creates a new instance.
    /// Starts new tasks for starting new connections and handling incoming messages.
    pub fn new(
        peer_configs: Vec<P2PPeerConfig>,
        signing_context: Arc<RelaySigningContext>,
    ) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(100);
        let (api_requests_tx, api_requests_rx) = mpsc::channel(2000);
        let this = Arc::new(Self { peer_configs, broadcast_tx, api_requests_tx, signing_context });
        for peer_config in &this.peer_configs {
            // Parse URL and try to turn into a request ahead-of-time, panicking on error
            let request = peer_config
                .url
                .as_str()
                .into_client_request()
                .inspect_err(|e| error!(err=?e, url=%peer_config.url, "invalid peer URL"))
                .expect("peer URL in config should be valid");

            tokio::spawn(this.clone().connect_to_peer(request, peer_config.verifying_key.clone()));
        }
        tokio::spawn(this.clone().handle_incoming_messages(api_requests_rx));
        this
    }

    #[tracing::instrument(skip_all)]
    pub async fn p2p_connect(
        Extension(api): Extension<Arc<P2PApi>>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, std::convert::Infallible> {
        info!("got new peer connection");
        // Upgrade connection to WebSocket, spawning a new task to handle the connection
        let ws = ws
            .on_failed_upgrade(|error| warn!(%error, "websocket upgrade failed"))
            .on_upgrade(|socket| api.on_peer_connection(socket.into()));

        Ok(ws)
    }

    async fn connect_to_peer(
        self: Arc<Self>,
        request: axum::http::Request<()>,
        pubkey: BlsPublicKey,
    ) {
        let pubkey_bytes = pubkey.serialize().into();
        // If the peer's pubkey is less than ours, don't try to connect.
        // Imposing an order on the pubkeys prevents redundant connections between peers.
        if pubkey_bytes <= self.signing_context.pubkey {
            return;
        }
        // Attempt to connect, waiting for a bit before retrying
        loop {
            let Ok((ws, _response)) = connect_async(request.clone()).await else {
                continue;
            };
            let _ = self.handle_ws_connection(ws.into(), Some((pubkey_bytes, pubkey.clone()))).await.inspect_err(|e| {
                error!(err=?e, direction="outbound", peer=%pubkey_bytes, "websocket communication error")
            });

            // TODO: change to an exponential backoff?
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    async fn on_peer_connection(self: Arc<Self>, socket: PeerSocket) {
        // If connection fails, log the error and return.
        let _ = self
            .handle_ws_connection(socket, None)
            .await
            .inspect_err(|e| error!(err=?e, direction="inbound", "websocket communication error"));
    }

    async fn handle_ws_connection(
        &self,
        mut socket: PeerSocket,
        mut peer_pubkey: Option<(BlsPublicKeyBytes, BlsPublicKey)>,
    ) -> Result<(), WsConnectionError> {
        let mut broadcast_rx = self.broadcast_tx.subscribe();
        // Send an initial Hello message
        let hello_message = RawP2PMessage::Hello(SignedHelloMessage::new(&self.signing_context));
        socket.send(hello_message.to_ws_message()).await.map_err(WsConnectionError::SendError)?;
        loop {
            tokio::select! {
                res = broadcast_rx.recv() => {
                    let Ok(msg) = res else {
                        // Sender was closed
                        return Ok(());
                    };
                    let serialized = RawP2PMessage::Other(msg).to_ws_message();
                    socket.send(serialized).await.map_err(WsConnectionError::SendError)?;
                },
                res = socket.next() => {
                    let Some(res) = res else {
                        // Socket was closed
                        return Ok(());
                    };
                    let msg = res.map_err(WsConnectionError::RecvError)?;
                    let message = RawP2PMessage::from_ws_message(&msg)?;
                    self.handle_raw_message(message, &mut peer_pubkey).await?;
                }
            };
        }
    }

    async fn handle_raw_message(
        &self,
        message: RawP2PMessage,
        peer_pubkey: &mut Option<(BlsPublicKeyBytes, BlsPublicKey)>,
    ) -> Result<(), WsConnectionError> {
        match message {
            RawP2PMessage::Hello(hello_msg) => {
                let pubkey = hello_msg.pubkey()?;
                // Check peer is known
                let known_peer =
                    self.peer_configs.iter().any(|config| config.verifying_key == pubkey);
                if !known_peer {
                    return Err(WsConnectionError::UnknownPeer(pubkey));
                }
                hello_msg.verify_signature(&pubkey)?;
                info!("Verified Hello message from peer: {pubkey}");

                let Some((_, peer_pubkey)) = &peer_pubkey else {
                    *peer_pubkey = Some((pubkey.serialize().into(), pubkey));
                    return Ok(());
                };
                if peer_pubkey != &pubkey {
                    return Err(WsConnectionError::UnexpectedPubkey(pubkey, peer_pubkey.clone()));
                }
            }
            RawP2PMessage::Other(message) => {
                let Some((sender, _)) = &peer_pubkey else {
                    return Err(WsConnectionError::NoHello);
                };
                let sender = *sender;
                self.api_requests_tx
                    .send(P2PApiRequest::PeerMessage { sender, message })
                    .await
                    .map_err(|_| WsConnectionError::ChannelClosed)?;
            }
        }

        Ok(())
    }

    async fn handle_incoming_messages(
        self: Arc<Self>,
        mut api_requests_rx: mpsc::Receiver<P2PApiRequest>,
    ) {
        const CUTOFF_TIME_1: Duration = Duration::from_secs(2);
        const CUTOFF_TIME_2: Duration = Duration::from_secs(2);
        let mut vote_map: HashMap<BlsPublicKeyBytes, (u64, InclusionList)> = HashMap::new();
        while let Some(request) = api_requests_rx.recv().await {
            match request {
                P2PApiRequest::LocalInclusionList(request) => {
                    let msg =
                        InclusionListMessage::new(request.slot, request.inclusion_list.clone());
                    self.broadcast(msg.into());

                    let api_requests_tx = self.api_requests_tx.clone();
                    let shared_il_msg = P2PApiRequest::SharedInclusionList(request);
                    tokio::spawn(async move {
                        // Sleep for t_1 time
                        tokio::time::sleep(CUTOFF_TIME_1).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SharedInclusionList(request) => {
                    let InclusionListRequest { slot, inclusion_list, .. } = request;
                    let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list);

                    let msg = InclusionListMessage::new(slot, shared_il.clone());
                    self.broadcast(msg.into());

                    let api_requests_tx = self.api_requests_tx.clone();
                    let settle_request =
                        InclusionListRequest { inclusion_list: shared_il, ..request };
                    let shared_il_msg = P2PApiRequest::SettledInclusionList(settle_request);
                    tokio::spawn(async move {
                        // Sleep for t_2 time
                        tokio::time::sleep(CUTOFF_TIME_2).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SettledInclusionList(request) => {
                    let InclusionListRequest { slot, inclusion_list, result_tx } = request;
                    let vote_map = std::mem::take(&mut vote_map);
                    let final_il = compute_final_inclusion_list(vote_map, slot, inclusion_list);
                    let _ = result_tx
                        .send(Some(final_il))
                        .inspect_err(|e| error!(err=?e, "failed to send settled inclusion list"));
                }
                P2PApiRequest::PeerMessage { sender, message } => {
                    let P2PMessage::InclusionList(il_msg) = message;
                    vote_map.insert(sender, (il_msg.slot, il_msg.inclusion_list));
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum WsConnectionError {
    #[error("failed to send message")]
    SendError(crate::socket::Error),
    #[error("failed to receive message")]
    RecvError(crate::socket::Error),
    #[error("failed to decode message")]
    DecodingError(#[from] EncodingError),
    #[error("first message from peer was not a Hello")]
    NoHello,
    #[error("got an unexpected pubkey, got: {_0}, expected: {_1}")]
    UnexpectedPubkey(BlsPublicKey, BlsPublicKey),
    #[error("not a known peer: {_0}")]
    UnknownPeer(BlsPublicKey),
    #[error("initial authentication failed: {_0}")]
    MessageAuthentication(#[from] MessageAuthenticationError),
    #[error("receiver was closed")]
    ChannelClosed,
}
