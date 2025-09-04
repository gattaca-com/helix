use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        WebSocketUpgrade,
    },
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    Extension,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use helix_common::{signing::RelaySigningContext, P2PPeerConfig};
use tokio::sync::{broadcast::Sender, mpsc};
use tokio_tungstenite::connect_async;
use tracing::error;

use crate::messages::{P2PMessage, SignedP2PMessage};

pub mod messages;

pub struct P2PApi {
    broadcast_tx: Sender<SignedP2PMessage>,
    peer_messages_tx: mpsc::Sender<P2PMessage>,
    peer_messages_rx: mpsc::Receiver<P2PMessage>,
    signing_context: Arc<RelaySigningContext>,
    peer_configs: Vec<P2PPeerConfig>,
}

impl P2PApi {
    pub async fn new(
        peer_configs: Vec<P2PPeerConfig>,
        signing_context: Arc<RelaySigningContext>,
    ) -> Arc<Self> {
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(100);
        let (peer_messages_tx, peer_messages_rx) = mpsc::channel(2000);
        let this = Arc::new(Self {
            peer_configs,
            broadcast_tx,
            peer_messages_tx,
            peer_messages_rx,
            signing_context,
        });
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
                this_clone.handle_ws_connection(ws, Some(peer_config)).await;
            });
        }
        this
    }

    pub fn broadcast(&self, message: P2PMessage) {
        let signed_message = message.sign(&self.signing_context);
        // Ignore error if there are no active receivers.
        let _ = self.broadcast_tx.send(signed_message);
    }

    #[tracing::instrument(skip_all)]
    pub async fn p2p_connect(
        Extension(api): Extension<Arc<P2PApi>>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, P2PApiError> {
        println!("got request");
        Ok(ws.on_upgrade(|socket| async move { api.handle_ws_connection(socket, None).await }))
    }

    async fn handle_ws_connection<M, S, E>(&self, mut socket: S, peer_config: Option<P2PPeerConfig>)
    where
        S: Stream<Item = Result<M, E>> + Sink<M> + Unpin,
        M: TryFrom<SignedP2PMessage> + std::fmt::Debug,
        SignedP2PMessage: TryFrom<M>,
        <M as TryFrom<SignedP2PMessage>>::Error: std::fmt::Debug,
        <SignedP2PMessage as TryFrom<M>>::Error: std::fmt::Debug,
        <S as futures::Sink<M>>::Error: std::fmt::Debug,
        E: std::fmt::Debug,
    {
        let mut broadcast_rx = self.broadcast_tx.subscribe();
        loop {
            tokio::select! {
                res = broadcast_rx.recv() => {
                    let Ok(msg) = res else {
                        // Sender was closed
                        break;
                    };
                    println!("sending message");
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
                    msg.verify_signature().unwrap();
                    self.peer_messages_tx.send(msg.message).await.unwrap();
                }
            };
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
