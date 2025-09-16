use std::{sync::Arc, time::Duration};

use axum::{extract::WebSocketUpgrade, response::IntoResponse, Extension};
use futures::{SinkExt, StreamExt};
use helix_types::BlsPublicKeyBytes;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, warn};

use crate::{
    event_handlers::NetworkEvent,
    messages::{
        EncodingError, HelloMessage, MessageAuthenticationError, NetworkMessage,
        NetworkMessageType, RawNetworkMessage,
    },
    socket::PeerSocket,
    RelayNetworkApi,
};

struct PeerInfo {
    pubkey: BlsPublicKeyBytes,
    supported_message_types: Vec<NetworkMessageType>,
}

impl PeerInfo {
    fn new(pubkey: BlsPublicKeyBytes) -> Self {
        Self { pubkey, supported_message_types: Vec::new() }
    }

    fn set_supported_message_types(&mut self, supported_message_types: Vec<NetworkMessageType>) {
        self.supported_message_types = supported_message_types;
    }

    fn supports_message(&self, message: &NetworkMessage) -> bool {
        self.supported_message_types.iter().any(|msg_type| msg_type.supports_message(message))
    }
}

impl RelayNetworkApi {
    #[tracing::instrument(skip_all)]
    pub async fn connect(
        Extension(api): Extension<Arc<RelayNetworkApi>>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, std::convert::Infallible> {
        debug!("got new peer connection");
        // Upgrade connection to WebSocket, spawning a new task to handle the connection
        let ws = ws
            .on_failed_upgrade(|error| warn!(%error, "websocket upgrade failed"))
            .on_upgrade(|socket| api.on_peer_connection(socket.into()));

        Ok(ws)
    }

    pub(crate) async fn connect_to_peer(
        self: Arc<Self>,
        request: axum::http::Request<()>,
        pubkey: BlsPublicKeyBytes,
    ) {
        // Attempt to connect, waiting for a bit before retrying
        loop {
            debug!(peer=%pubkey, "connecting to peer");

            let connection_result = connect_async(request.clone())
                .await
                .inspect_err(|e| warn!(err=%e,peer=%pubkey, "failed to connect to peer"));

            if let Ok((ws, _response)) = connection_result {
                let _ = self.handle_ws_connection(ws.into(), Some(pubkey)).await.inspect_err(|e| {
                   warn!(err=?e, direction="outbound", peer=%pubkey, "websocket communication error")
               });
            }

            // TODO: change to an exponential backoff?
            tokio::time::sleep(Duration::from_secs(8)).await;
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
        peer_pubkey: Option<BlsPublicKeyBytes>,
    ) -> Result<(), WsConnectionError> {
        let mut peer_info = peer_pubkey.map(PeerInfo::new);
        let mut broadcast_rx = self.broadcast_tx.subscribe();
        // Send an initial Hello message
        let hello_message = RawNetworkMessage::Hello(
            HelloMessage::new(&self.signing_context)
                .with_supported_message_types(vec![NetworkMessageType::InclusionList]),
        );
        socket.send(hello_message.to_ws_message()).await.map_err(WsConnectionError::SendError)?;
        loop {
            tokio::select! {
                res = broadcast_rx.recv() => {
                    let Ok(msg) = res else {
                        // Sender was closed
                        return Ok(());
                    };
                    if peer_supports_message_type(&peer_info, &msg) {
                        let serialized = RawNetworkMessage::Other(msg).to_ws_message();
                        socket.send(serialized).await.map_err(WsConnectionError::SendError)?;
                    }
                },
                res = socket.next() => {
                    let Some(res) = res else {
                        // Socket was closed
                        return Ok(());
                    };
                    let msg = res.map_err(WsConnectionError::RecvError)?;
                    let message = RawNetworkMessage::from_ws_message(&msg)?;
                    self.handle_raw_message(message, &mut peer_info).await?;
                }
            };
        }
    }

    async fn handle_raw_message(
        &self,
        message: RawNetworkMessage,
        opt_peer_info: &mut Option<PeerInfo>,
    ) -> Result<(), WsConnectionError> {
        match message {
            RawNetworkMessage::Hello(hello_msg) => {
                let pubkey = hello_msg.pubkey;
                // Sanity check: we're not talking to ourselves
                if pubkey == self.signing_context.pubkey {
                    return Err(WsConnectionError::ReceivedOwnPubkey);
                }
                // Check peer is known
                let known_peer =
                    self.network_config.peers.iter().any(|config| config.pubkey == pubkey);
                if !known_peer {
                    return Err(WsConnectionError::UnknownPeer(pubkey));
                }
                {
                    let deserialized_pubkey = hello_msg.deserialize_pubkey()?;
                    hello_msg.verify_signature(&deserialized_pubkey)?;
                }
                debug!(peer=%pubkey, "Verified Hello message from peer");

                let supported_message_types = hello_msg.into_supported_message_types();

                let Some(peer_info) = opt_peer_info else {
                    let mut peer_info = PeerInfo::new(pubkey);
                    peer_info.set_supported_message_types(supported_message_types);
                    *opt_peer_info = Some(peer_info);
                    return Ok(());
                };
                if peer_info.pubkey != pubkey {
                    return Err(WsConnectionError::UnexpectedPubkey(pubkey, peer_info.pubkey));
                }
                // Override previously known supported message types
                peer_info.set_supported_message_types(supported_message_types);
            }
            RawNetworkMessage::Other(message) => {
                let Some(peer_info) = &opt_peer_info else {
                    return Err(WsConnectionError::NoHello);
                };
                let sender = peer_info.pubkey;
                self.api_events_tx
                    .send(NetworkEvent::PeerMessage { sender, message })
                    .await
                    .map_err(|_| WsConnectionError::ChannelClosed)?;
            }
        }

        Ok(())
    }

    pub(crate) fn broadcast(&self, message: NetworkMessage) {
        // Ignore error if there are no active receivers.
        let _ = self.broadcast_tx.send(message);
    }
}

fn peer_supports_message_type(opt_peer_info: &Option<PeerInfo>, message: &NetworkMessage) -> bool {
    let Some(peer_info) = opt_peer_info else {
        return false;
    };
    peer_info.supports_message(message)
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
    UnexpectedPubkey(BlsPublicKeyBytes, BlsPublicKeyBytes),
    #[error("peer sent our own pubkey in Hello message")]
    ReceivedOwnPubkey,
    #[error("not a known peer: {_0}")]
    UnknownPeer(BlsPublicKeyBytes),
    #[error("initial authentication failed: {_0}")]
    MessageAuthentication(#[from] MessageAuthenticationError),
    #[error("receiver was closed")]
    ChannelClosed,
}
