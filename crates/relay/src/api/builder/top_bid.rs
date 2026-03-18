use std::{io::ErrorKind, sync::Arc};

use axum::{Extension, response::IntoResponse};
use bytes::Bytes;
use crossbeam_channel::Receiver;
use flux::{tile::Tile, timing::Nanos};
use helix_common::{self, api::builder_api::TopBidUpdate, metrics::TopBidMetrics};
use hyper::HeaderMap;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tracing::{debug, error};

use super::api::BuilderApi;
use crate::{
    HelixSpine,
    api::{
        Api, HEADER_API_KEY, HEADER_API_TOKEN,
        builder::error::BuilderApiError,
        extract::raw_web_socket::{RawWebSocket, RawWebSocketUpgrade},
    },
};

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: RawWebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let Some(api_key) = headers
            .get(HEADER_API_KEY)
            .or_else(|| headers.get(HEADER_API_TOKEN))
            .and_then(|key| key.to_str().ok())
        else {
            return Err(BuilderApiError::InvalidApiKey);
        };

        if !api.local_cache.contains_api_key(api_key) {
            return Err(BuilderApiError::InvalidApiKey);
        }

        let sender = api.web_socket_connections.clone();
        Ok(ws.on_upgrade(move |socket| {
            if let Err(e) = sender.try_send(socket) {
                tracing::error!(error=?e, "failed to send new web socket connection to top bid tile");
            }
        }))
    }
}

pub struct TopBidTile {
    new_connections: Receiver<RawWebSocket>,
    connections: Vec<RawWebSocket>,
    last_send: Nanos,
}

impl TopBidTile {
    pub fn new(new_connections: Receiver<RawWebSocket>) -> Self {
        Self { new_connections, connections: vec![], last_send: Nanos::now() }
    }
}

impl Tile<HelixSpine> for TopBidTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        // add any new connections
        while let Ok(web_socket) = self.new_connections.try_recv() {
            let _ = TopBidMetrics::connection();
            self.connections.push(web_socket);
        }

        // send top bids - note that `send` call is non-blocking.
        adapter.consume(|top_bid: TopBidUpdate, _producers| {
            TopBidMetrics::top_bid_update_count();

            let mut i = 0;
            while i < self.connections.len() {
                match self.connections[i].send(Message::Binary(top_bid.as_ssz_bytes_fast().into())) {
                    Ok(_) => i += 1,
                    Err(e) => {
                        error!(error=?e, peer=?self.connections[i].get_ref().peer_addr(), "Failed to send bid. Disconnecting.");
                        self.connections.swap_remove(i);
                    }
                }
            }
            self.last_send = Nanos::now();
        });

        if self.last_send.elapsed() > Nanos::from_secs(10) {
            // Send a ping
            let mut i = 0;
            while i < self.connections.len() {
                match self.connections[i].send(Message::Ping(Bytes::new())) {
                    Ok(_) => i += 1,
                    Err(e) => {
                        error!(error=?e, peer=?self.connections[i].get_ref().peer_addr(), "Failed to send ping. Disconnecting.");
                        self.connections.swap_remove(i);
                    }
                }
            }
            self.last_send = Nanos::now();
        }

        // Read incoming
        let mut i = 0;
        while i < self.connections.len() {
            match self.connections[i].read() {
                Ok(msg) => match msg {
                    Message::Ping(data) => match self.connections[i].send(Message::Ping(data)) {
                        Ok(_) => i += 1,
                        Err(e) => {
                            error!(error=?e, peer=?self.connections[i].get_ref().peer_addr(), "Failed to send pong. Disconnecting.");
                            self.connections.swap_remove(i);
                        }
                    },
                    Message::Close(_) => {
                        debug!("Received close frame.");
                        self.connections.swap_remove(i);
                    }
                    _ => i += 1,
                },
                Err(WebSocketError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    i += 1;
                }
                Err(e) => {
                    error!(error=?e, peer=?self.connections[i].get_ref().peer_addr(), "Failed to read. Disconnecting.");
                    self.connections.swap_remove(i);
                }
            }
        }
    }
}
