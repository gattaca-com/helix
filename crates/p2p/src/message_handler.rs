use std::{sync::Arc, time::Duration};

use axum::{extract::WebSocketUpgrade, response::IntoResponse, Extension};
use futures::{SinkExt, StreamExt};
use helix_common::{signing::RelaySigningContext, P2PPeerConfig};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use tracing::{error, info, warn};

use crate::{
    messages::{EncodingError, MessageAuthenticationError, RawP2PMessage, SignedHelloMessage},
    request_handlers::P2PApiRequest,
    socket::PeerSocket,
    P2PApi,
};

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
            let peer_pubkey = peer_config.pubkey;
            // Parse URL and try to turn into a request ahead-of-time, panicking on error
            let request = url_to_client_request(&peer_config.url);
            // Verify serialized public key is valid
            let pubkey = BlsPublicKey::deserialize(peer_pubkey.as_ref())
                .inspect_err(
                    |e| error!(err=?e, pubkey=%peer_pubkey, "failed to deserialize peer pubkey"),
                )
                .expect("pubkey should be valid");

            // If the peer's pubkey is less than ours, don't try to connect.
            // Imposing an order on the pubkeys prevents redundant connections between peers.
            if peer_pubkey > this.signing_context.pubkey {
                tokio::spawn(this.clone().connect_to_peer(request, pubkey, peer_pubkey));
            }
        }
        tokio::spawn(this.clone().handle_requests(api_requests_rx));
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
        pubkey_bytes: BlsPublicKeyBytes,
    ) {
        // Attempt to connect, waiting for a bit before retrying
        loop {
            if let Ok((ws, _response)) = connect_async(request.clone()).await {
                let _ = self.handle_ws_connection(ws.into(), Some((pubkey_bytes, pubkey.clone()))).await.inspect_err(|e| {
                   error!(err=?e, direction="outbound", peer=%pubkey_bytes, "websocket communication error")
               });
            }

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
                let pubkey_bytes = hello_msg.message.pubkey;
                // Check peer is known
                let known_peer =
                    self.peer_configs.iter().any(|config| config.pubkey == pubkey_bytes);
                if !known_peer {
                    return Err(WsConnectionError::UnknownPeer(pubkey_bytes));
                }
                let pubkey = hello_msg.deserialize_pubkey()?;
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
}

fn url_to_client_request(url: &str) -> axum::http::Request<()> {
    let request = url
        .into_client_request()
        .inspect_err(|e| error!(err=?e, %url, "invalid peer URL"))
        .expect("peer URL in config should be valid");
    request
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
    UnknownPeer(BlsPublicKeyBytes),
    #[error("initial authentication failed: {_0}")]
    MessageAuthentication(#[from] MessageAuthenticationError),
    #[error("receiver was closed")]
    ChannelClosed,
}
