use std::{sync::Arc, time::Duration};

use axum::{
    Extension,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::StreamExt;
use helix_common::{self, metrics::TopBidMetrics};
use hyper::HeaderMap;
use tokio::time::{self};
use tracing::{debug, error};

use super::api::BuilderApi;
use crate::{Api, HEADER_API_KEY, builder::error::BuilderApiError};

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let Some(api_key) = headers.get(HEADER_API_KEY) else {
            return Err(BuilderApiError::InvalidApiKey);
        };

        if !api.local_cache.contains_api_key(api_key) {
            return Err(BuilderApiError::InvalidApiKey);
        }

        let sub = api.top_bid_tx.subscribe();
        Ok(ws.on_upgrade(move |socket| push_top_bids(socket, sub)))
    }
}

/// `push_top_bids` manages a WebSocket connection to continuously send the top auction bids to a
/// client.
///
/// - Periodically fetches the latest auction bids via a stream and sends them to the client in ssz
///   format.
/// - Sends a ping message every 10 seconds to maintain the connection's liveliness.
/// - Terminates the connection on sending failures or if a bid stream error occurs, ensuring clean
///   disconnection.
///
/// This function operates in an asynchronous loop until the WebSocket connection is closed either
/// due to an error or when the auction ends. It returns after the socket has been closed, logging
/// the closure status.
async fn push_top_bids(
    mut socket: WebSocket,
    mut bid_stream: tokio::sync::broadcast::Receiver<Bytes>,
) {
    let _conn = TopBidMetrics::connection();
    let mut interval = time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            Ok(bid) = bid_stream.recv() => {
                if socket.send(Message::Binary(bid)).await.is_err() {
                    error!("Failed to send bid. Disconnecting.");
                    break;
                }
            },

            _ = interval.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    error!("Failed to send ping.");
                    break;
                }
            },

            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            error!("Failed to respond to ping.");
                            break;
                        }
                    },
                    Some(Ok(Message::Pong(_))) => {
                        debug!("Received pong response.");
                    },
                    Some(Ok(Message::Close(_))) => {
                        debug!("Received close frame.");
                        break;
                    },
                    Some(Ok(Message::Binary(_))) => {
                        debug!("Received Binary frame.");
                    },
                    Some(Ok(Message::Text(_))) => {
                        debug!("Received Text frame.");
                    },
                    Some(Err(e)) => {
                        error!("Error in WebSocket connection: {}", e);
                        break;
                    },
                    None => {
                        error!("WebSocket connection closed by the other side.");
                        break;
                    }
                }
            }
        }
    }

    debug!("Socket connection closed gracefully.");
}
