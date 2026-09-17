use std::{io::ErrorKind, os::fd::AsRawFd, sync::Arc, time::Duration};

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
use mio::{Events, Interest, Poll, Registry, Token, unix::SourceFd};
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

struct Connection {
    ws: RawWebSocket,
    token: Token,
    precision: TopBidPrecision,
    _metrics: TopBidMetrics,
}

impl Connection {
    fn deregister(&mut self, registry: &Registry) {
        let fd = self.ws.get_ref().as_raw_fd();
        let _ = registry.deregister(&mut SourceFd(&fd));
    }
}

pub struct TopBidTile {
    new_connections: Receiver<(RawWebSocket, TopBidPrecision)>,
    connections: Vec<Connection>,
    poll: Poll,
    events: Events,
    next_token: usize,
    last_send: Nanos,
    bid_slot: u64,
    stats: SlotStats,
}

impl TopBidTile {
    pub fn new(new_connections: Receiver<(RawWebSocket, TopBidPrecision)>) -> Self {
        Self {
            new_connections,
            connections: vec![],
            poll: Poll::new().expect("failed to create top bid poll"),
            events: Events::with_capacity(256),
            next_token: 0,
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

    fn accept_new(&mut self) {
        while let Ok((ws, precision)) = self.new_connections.try_recv() {
            let token = Token(self.next_token);
            self.next_token += 1;
            let fd = ws.get_ref().as_raw_fd();
            if let Err(e) =
                self.poll.registry().register(&mut SourceFd(&fd), token, Interest::READABLE)
            {
                error!(error=?e, peer=?ws.get_ref().peer_addr(), "Failed to register web socket. Dropping.");
                continue;
            }
            let _metrics = TopBidMetrics::connection();
            self.connections.push(Connection { ws, token, precision, _metrics });
            self.stats.new_connections += 1;
        }
    }

    fn disconnect(&mut self, i: usize) {
        self.connections[i].deregister(self.poll.registry());
        self.connections.swap_remove(i);
    }

    /// `send` is non-blocking: a full kernel send buffer surfaces as an error and
    /// the connection is dropped rather than stalling the fan-out.
    fn broadcast(&mut self, top_bid: TopBidUpdate) {
        self.on_top_bid_slot(top_bid.slot);
        self.stats.top_bid_updates_received += 1;

        let mut millis: Option<Bytes> = None;
        let mut nanos: Option<Bytes> = None;
        let mut i = 0;
        while i < self.connections.len() {
            let precision = self.connections[i].precision;
            let payload = match precision {
                TopBidPrecision::Millis => &mut millis,
                TopBidPrecision::Nanos => &mut nanos,
            }
            .get_or_insert_with(|| top_bid.as_ssz_bytes_with_precision(precision))
            .clone();
            match self.connections[i].ws.send(Message::Binary(payload)) {
                Ok(_) => {
                    self.stats.sends_ok += 1;
                    i += 1;
                }
                Err(e) => {
                    self.stats.sends_failed += 1;
                    error!(error=?e, peer=?self.connections[i].ws.get_ref().peer_addr(), "Failed to send bid. Disconnecting.");
                    self.disconnect(i);
                }
            }
        }
        self.last_send = Nanos::now();
    }

    fn maybe_ping(&mut self) {
        if self.last_send.elapsed() <= Nanos::from_secs(10) {
            return;
        }
        let mut i = 0;
        while i < self.connections.len() {
            match self.connections[i].ws.send(Message::Ping(Bytes::new())) {
                Ok(_) => {
                    self.stats.pings_sent += 1;
                    i += 1;
                }
                Err(e) => {
                    self.stats.pings_failed += 1;
                    error!(error=?e, peer=?self.connections[i].ws.get_ref().peer_addr(), "Failed to send ping. Disconnecting.");
                    self.disconnect(i);
                }
            }
        }
        self.last_send = Nanos::now();
    }

    /// One `epoll_wait` per loop instead of one `recv` per connection. Readiness
    /// is edge-triggered, so a ready socket is drained until `WouldBlock`.
    fn poll_reads(&mut self) {
        let Self { poll, events, connections, stats, .. } = self;
        if let Err(e) = poll.poll(events, Some(Duration::ZERO)) {
            if e.kind() != ErrorKind::Interrupted {
                error!(error=?e, "top bid poll failed");
            }
            return;
        }
        let registry = poll.registry();
        for event in events.iter() {
            let Some(i) = connections.iter().position(|c| c.token == event.token()) else {
                continue;
            };
            loop {
                match connections[i].ws.read() {
                    Ok(Message::Ping(data)) => match connections[i].ws.send(Message::Pong(data)) {
                        Ok(_) => stats.pongs_sent += 1,
                        Err(e) => {
                            stats.pongs_failed += 1;
                            error!(error=?e, peer=?connections[i].ws.get_ref().peer_addr(), "Failed to send pong. Disconnecting.");
                            connections[i].deregister(registry);
                            connections.swap_remove(i);
                            break;
                        }
                    },
                    Ok(Message::Close(_)) => {
                        debug!("Received close frame.");
                        stats.closes_received += 1;
                        connections[i].deregister(registry);
                        connections.swap_remove(i);
                        break;
                    }
                    Ok(_) => {}
                    Err(WebSocketError::Io(e)) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        stats.read_errors += 1;
                        error!(error=?e, peer=?connections[i].ws.get_ref().peer_addr(), "Failed to read. Disconnecting.");
                        connections[i].deregister(registry);
                        connections.swap_remove(i);
                        break;
                    }
                }
            }
        }
    }
}

impl Tile<HelixSpine> for TopBidTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.accept_new();
        adapter.consume(|top_bid: TopBidUpdate, _producers| self.broadcast(top_bid));
        self.maybe_ping();
        self.poll_reads();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, B256, U256};
    use helix_types::BlsPublicKeyBytes;
    use ssz::Decode;
    use tokio_tungstenite::tungstenite::{WebSocket, accept, client};

    use super::*;

    type Client = WebSocket<TcpStream>;

    fn connect(
        tile: &mut TopBidTile,
        tx: &crossbeam_channel::Sender<(RawWebSocket, TopBidPrecision)>,
        precision: TopBidPrecision,
    ) -> Client {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            client(format!("ws://{addr}"), stream).unwrap().0
        });
        let (stream, _) = listener.accept().unwrap();
        let server = accept(stream).unwrap();
        server.get_ref().set_nonblocking(true).unwrap();
        tx.send((server, precision)).unwrap();
        tile.accept_new();
        handle.join().unwrap()
    }

    fn update() -> TopBidUpdate {
        TopBidUpdate {
            timestamp: 1_700_000_000_123_456_789,
            slot: 42,
            block_number: 7,
            block_hash: B256::repeat_byte(1),
            parent_hash: B256::repeat_byte(2),
            builder_pubkey: BlsPublicKeyBytes::default(),
            fee_recipient: Address::repeat_byte(3),
            value: U256::from(5u64),
        }
    }

    fn poll_until(tile: &mut TopBidTile, mut done: impl FnMut(&TopBidTile) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(tile) {
            assert!(Instant::now() < deadline, "timed out waiting for tile state");
            tile.poll_reads();
        }
    }

    fn read_binary(client: &mut Client) -> TopBidUpdate {
        match client.read().unwrap() {
            Message::Binary(b) => TopBidUpdate::from_ssz_bytes(&b).unwrap(),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[test]
    fn broadcast_fans_out_per_precision() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let mut tile = TopBidTile::new(rx);
        let mut nanos = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        let mut millis = connect(&mut tile, &tx, TopBidPrecision::Millis);
        let mut nanos2 = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        assert_eq!(tile.connections.len(), 3);
        assert_eq!(tile.stats.new_connections, 3);

        let update = update();
        tile.broadcast(update);
        assert_eq!(tile.stats.sends_ok, 3);
        assert_eq!(tile.stats.sends_failed, 0);
        assert_eq!(tile.bid_slot, update.slot);

        assert_eq!(read_binary(&mut nanos).timestamp, update.timestamp);
        assert_eq!(read_binary(&mut nanos2).timestamp, update.timestamp);
        assert_eq!(read_binary(&mut millis).timestamp, update.timestamp / 1_000_000);
    }

    #[test]
    fn poll_reads_answers_ping_and_drops_on_close() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let mut tile = TopBidTile::new(rx);
        let mut client = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        let _idle = connect(&mut tile, &tx, TopBidPrecision::Nanos);

        client.send(Message::Ping(Bytes::from_static(b"hi"))).unwrap();
        poll_until(&mut tile, |t| t.stats.pongs_sent == 1);
        assert!(matches!(client.read().unwrap(), Message::Pong(_)));

        client.close(None).unwrap();
        client.flush().unwrap();
        poll_until(&mut tile, |t| t.connections.len() == 1);
        assert_eq!(tile.stats.closes_received, 1);
        assert_eq!(tile.stats.read_errors, 0);
    }

    #[test]
    fn poll_reads_drops_reset_peer() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let mut tile = TopBidTile::new(rx);
        let client = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        drop(client);
        poll_until(&mut tile, |t| t.connections.is_empty());
        assert_eq!(tile.stats.read_errors, 1);
    }

    #[test]
    fn broadcast_after_disconnect_reaches_remaining() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        let mut tile = TopBidTile::new(rx);
        let gone = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        let mut stays = connect(&mut tile, &tx, TopBidPrecision::Nanos);
        drop(gone);
        poll_until(&mut tile, |t| t.connections.len() == 1);

        tile.broadcast(update());
        assert_eq!(tile.stats.sends_ok, 1);
        assert_eq!(read_binary(&mut stays).slot, 42);
    }
}
