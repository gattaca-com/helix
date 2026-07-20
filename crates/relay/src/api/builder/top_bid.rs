use std::{io::ErrorKind, sync::Arc};

use axum::{Extension, response::IntoResponse};
use bytes::Bytes;
use crossbeam_channel::Receiver;
use flux::{tile::Tile, timing::Nanos};
use helix_common::{
    self,
    api::builder_api::{TopBidPrecision, TopBidUpdate},
    metrics::TopBidMetrics,
};
use hyper::HeaderMap;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tracing::{debug, error, info};

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
        api: Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: RawWebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        Self::connect_top_bid(api, headers, ws, TopBidPrecision::Millis)
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid_v2(
        api: Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: RawWebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        Self::connect_top_bid(api, headers, ws, TopBidPrecision::Nanos)
    }

    fn connect_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: RawWebSocketUpgrade,
        precision: TopBidPrecision,
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
            if let Err(e) = sender.try_send((socket, precision)) {
                tracing::error!(error=?e, "failed to send new web socket connection to top bid tile");
            }
        }))
    }
}

/// Per-slot counters, logged and reset when `TopBidUpdate.slot` advances.
/// Sends/pings are per-(update, connection) fan-out, a different unit from
/// `top_bid_updates_received`, which is per-update.
#[derive(Default)]
struct SlotStats {
    new_connections: u32,
    top_bid_updates_received: u32,
    sends_ok: u32,
    sends_failed: u32,
    pings_sent: u32,
    pings_failed: u32,
    pongs_sent: u32,
    pongs_failed: u32,
    closes_received: u32,
    read_errors: u32,
}

pub struct TopBidTile {
    new_connections: Receiver<(RawWebSocket, TopBidPrecision)>,
    connections: Vec<(RawWebSocket, TopBidMetrics, TopBidPrecision)>,
    last_send: Nanos,
    bid_slot: u64,
    stats: SlotStats,
}

impl TopBidTile {
    pub fn new(new_connections: Receiver<(RawWebSocket, TopBidPrecision)>) -> Self {
        Self {
            new_connections,
            connections: vec![],
            last_send: Nanos::now(),
            bid_slot: 0,
            stats: SlotStats::default(),
        }
    }

    fn on_top_bid_slot(&mut self, slot: u64) {
        if slot <= self.bid_slot {
            return;
        }
        self.report_slot_stats();
        self.bid_slot = slot;
    }

    fn report_slot_stats(&mut self) {
        if self.bid_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut self.stats);
        info!(
            bid_slot = self.bid_slot,
            connections = self.connections.len(),
            new_connections = stats.new_connections,
            top_bid_updates_received = stats.top_bid_updates_received,
            sends_ok = stats.sends_ok,
            sends_failed = stats.sends_failed,
            pings_sent = stats.pings_sent,
            pings_failed = stats.pings_failed,
            pongs_sent = stats.pongs_sent,
            pongs_failed = stats.pongs_failed,
            closes_received = stats.closes_received,
            read_errors = stats.read_errors,
            "top bid slot stats"
        );
    }
}

impl Tile<HelixSpine> for TopBidTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        // add any new connections
        while let Ok((web_socket, precision)) = self.new_connections.try_recv() {
            let metric = TopBidMetrics::connection();
            self.connections.push((web_socket, metric, precision));
            self.stats.new_connections += 1;
        }

        // send top bids - note that `send` call is non-blocking.
        adapter.consume(|top_bid: TopBidUpdate, _producers| {
            self.on_top_bid_slot(top_bid.slot);
            self.stats.top_bid_updates_received += 1;
            let mut i = 0;
            while i < self.connections.len() {
                let precision = self.connections[i].2;
                let payload = top_bid.as_ssz_bytes_with_precision(precision);
                match self.connections[i].0.send(Message::Binary(payload)) {
                    Ok(_) => {
                        self.stats.sends_ok += 1;
                        i += 1;
                    }
                    Err(e) => {
                        self.stats.sends_failed += 1;
                        error!(error=?e, peer=?self.connections[i].0.get_ref().peer_addr(), "Failed to send bid. Disconnecting.");
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
                match self.connections[i].0.send(Message::Ping(Bytes::new())) {
                    Ok(_) => {
                        self.stats.pings_sent += 1;
                        i += 1;
                    }
                    Err(e) => {
                        self.stats.pings_failed += 1;
                        error!(error=?e, peer=?self.connections[i].0.get_ref().peer_addr(), "Failed to send ping. Disconnecting.");
                        self.connections.swap_remove(i);
                    }
                }
            }
            self.last_send = Nanos::now();
        }

        // Read incoming
        let mut i = 0;
        while i < self.connections.len() {
            match self.connections[i].0.read() {
                Ok(msg) => match msg {
                    Message::Ping(data) => match self.connections[i].0.send(Message::Pong(data)) {
                        Ok(_) => {
                            self.stats.pongs_sent += 1;
                            i += 1;
                        }
                        Err(e) => {
                            self.stats.pongs_failed += 1;
                            error!(error=?e, peer=?self.connections[i].0.get_ref().peer_addr(), "Failed to send pong. Disconnecting.");
                            self.connections.swap_remove(i);
                        }
                    },
                    Message::Close(_) => {
                        debug!("Received close frame.");
                        self.stats.closes_received += 1;
                        self.connections.swap_remove(i);
                    }
                    _ => i += 1,
                },
                Err(WebSocketError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    i += 1;
                }
                Err(e) => {
                    self.stats.read_errors += 1;
                    error!(error=?e, peer=?self.connections[i].0.get_ref().peer_addr(), "Failed to read. Disconnecting.");
                    self.connections.swap_remove(i);
                }
            }
        }
    }
}
