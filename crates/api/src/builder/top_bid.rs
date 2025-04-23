use std::{sync::Arc, time::Duration};

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use bytes::Bytes;
use futures::StreamExt;
use helix_common::{self, metadata_provider::MetadataProvider};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use hyper::HeaderMap;
use tokio::time::{self};
use tracing::{debug, error};

use super::api::BuilderApi;
use crate::{
    builder::{error::BuilderApiError, traits::BlockSimulator},
    gossiper::traits::GossipClientTrait,
};

impl<A, DB, S, G, MP> BuilderApi<A, DB, S, G, MP>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
    MP: MetadataProvider + 'static,
{
    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G, MP>>>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let api_key = headers.get("x-api-key").and_then(|key| key.to_str().ok());
        match api_key {
            Some(key) => match api.db.check_builder_api_key(key).await {
                Ok(true) => {}
                Ok(false) => return Err(BuilderApiError::InvalidApiKey),
                Err(err) => {
                    error!(%err, "failed to check api key");
                    return Err(BuilderApiError::InternalError);
                }
            },
            None => return Err(BuilderApiError::InvalidApiKey),
        }

        Ok(ws.on_upgrade(move |socket| push_top_bids(socket, api.auctioneer.clone())))
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
async fn push_top_bids<A: Auctioneer + 'static>(mut socket: WebSocket, auctioneer: Arc<A>) {
    let mut bid_stream = auctioneer.get_best_bids().await;
    let mut interval = time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            Some(result) = bid_stream.next() => {
                match result {
                    Ok(bid) => {
                        if socket.send(Message::Binary(bid.into())).await.is_err() {
                            error!("Failed to send bid. Disconnecting.");
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Error while receiving bid: {}", e);
                        break;
                    }
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
