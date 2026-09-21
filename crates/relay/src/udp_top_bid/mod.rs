use std::{net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use flux::{
    tile::Tile,
    timing::{Duration, Instant},
};
use flux_network::{
    NetworkDriver, PollEvent, SendBehavior, Token, Transport, UdpConfig, tcp::TcpTelemetry,
};
use helix_common::{api::builder_api::TopBidUpdate, is_local_dev};
use helix_tcp_types::RegistrationMsg;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
use tracing::info;
use uuid::Uuid;

use crate::HelixSpine;

/// A session that has not sent its registration within this window is dropped.
const REGISTRATION_TIMEOUT_MS: u64 = 1_000;

/// Top bids are a latest-value feed: a peer that cannot keep up is better cut
/// than fed a slot of stale tops after its window drains. Keep the window at
/// the flux minimum and cut anything that backs up past it.
const SEND_WINDOW: usize = 64;
const MAX_BACKLOG: usize = 64;
const MAX_BACKLOG_TIMEOUT_MS: u64 = 100;
/// A `TopBidUpdate` is 188 bytes and a `RegistrationMsg` is 64; the cap only
/// has to stay under `SEND_WINDOW` datagrams, which flux asserts.
const MAX_MESSAGE_SIZE: usize = 4096;

/// `registration_ok + registration_invalid + registration_timeouts <=
/// accepted`; `sends` is per-(update, peer), `updates_received` is per-update.
#[derive(Default)]
struct Stats {
    accepted: u32,
    disconnected: u32,
    registration_ok: u32,
    registration_invalid: u32,
    registration_timeouts: u32,
    updates_received: u32,
    sends: u32,
}

pub struct UdpTopBidTile {
    driver: NetworkDriver,
    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    /// Authenticated peers, the only tokens an update is written to.
    registered: Vec<Token>,
    /// Accepted but not yet registered, with their accept time.
    pending: Vec<(Token, Instant)>,
    to_disconnect: Vec<Token>,
    /// Scratch for formatting an api key into the cache's `&str` key.
    key_buf: [u8; uuid::fmt::Hyphenated::LENGTH],

    bid_slot: u64,
    stats: Stats,
}

impl UdpTopBidTile {
    pub fn new(
        listener_addr: SocketAddr,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
    ) -> Self {
        let udp = UdpConfig {
            send_window: SEND_WINDOW,
            max_message_size: MAX_MESSAGE_SIZE,
            ..UdpConfig::wan()
        };
        let mut driver = NetworkDriver::default()
            .with_transport(Transport::Udp(udp))
            .with_telemetry(TcpTelemetry::Disabled)
            .with_socket_buf_size(8 * 1024 * 1024)
            .with_max_backlog(MAX_BACKLOG, Duration::from_millis(MAX_BACKLOG_TIMEOUT_MS));
        driver.listen_at(listener_addr).expect("failed to initialise the UDP top bid listener");

        Self {
            driver,
            api_key_cache,
            registered: Vec::with_capacity(max_connections),
            pending: Vec::with_capacity(max_connections),
            to_disconnect: Vec::with_capacity(max_connections),
            key_buf: [0; uuid::fmt::Hyphenated::LENGTH],
            bid_slot: 0,
            stats: Stats::default(),
        }
    }

    fn report_slot_stats(&mut self) {
        if self.bid_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut self.stats);
        // TODO: move to datagatherer telem
        info!(
            bid_slot = self.bid_slot,
            peers = self.registered.len(),
            accepted = stats.accepted,
            disconnected = stats.disconnected,
            registration_ok = stats.registration_ok,
            registration_invalid = stats.registration_invalid,
            registration_timeouts = stats.registration_timeouts,
            updates_received = stats.updates_received,
            sends = stats.sends,
            "udp top bid slot stats"
        );
    }
}

/// Same check as the TCP bid listener: the key must be known and cover the
/// claimed builder pubkey.
fn is_authorised(
    cache: &DashMap<String, Vec<BlsPublicKeyBytes>>,
    api_key: &str,
    builder_pubkey: &BlsPublicKeyBytes,
) -> bool {
    cache.get(api_key).is_some_and(|p| p.value().contains(builder_pubkey)) ||
        (is_local_dev() && cache.contains_key(api_key))
}

impl Tile<HelixSpine> for UdpTopBidTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        let Self {
            driver, api_key_cache, registered, pending, to_disconnect, key_buf, stats, ..
        } = self;

        driver.poll_with(|event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                tracing::trace!(?stream, %peer_addr, "udp top bid peer accepted");
                stats.accepted += 1;
                pending.push((stream, Instant::now()));
            }
            PollEvent::Reconnect { .. } => {}
            PollEvent::Disconnect { token } => {
                tracing::trace!(?token, "udp top bid peer disconnected");
                stats.disconnected += 1;
                registered.retain(|t| *t != token);
                pending.retain(|(t, _)| *t != token);
            }
            PollEvent::Message { token, payload, send_ts: _ } => {
                let Some(ix) = pending.iter().position(|(t, _)| *t == token) else {
                    // Registered peers have nothing to say on this feed.
                    return;
                };
                pending.swap_remove(ix);
                let msg = match RegistrationMsg::from_ssz_bytes(payload) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::error!(err=?e, "invalid udp registration message");
                        stats.registration_invalid += 1;
                        to_disconnect.push(token);
                        return;
                    }
                };

                let api_key = Uuid::from_bytes(msg.api_key).as_hyphenated().encode_lower(key_buf);
                if is_authorised(api_key_cache, api_key, &msg.builder_pubkey) {
                    stats.registration_ok += 1;
                    registered.push(token);
                } else {
                    tracing::error!(
                        %api_key,
                        builder_pubkey = %msg.builder_pubkey,
                        "unknown api key and pubkey pair, disconnecting udp peer"
                    );
                    stats.registration_invalid += 1;
                    to_disconnect.push(token);
                }
            }
        });

        let Self { pending, to_disconnect, stats, .. } = self;
        pending.retain(|(token, accepted_at)| {
            if accepted_at.elapsed() <= Duration::from_millis(REGISTRATION_TIMEOUT_MS) {
                return true;
            }
            stats.registration_timeouts += 1;
            to_disconnect.push(*token);
            false
        });

        for token in self.to_disconnect.drain(..) {
            self.driver.disconnect(token);
            self.registered.retain(|t| *t != token);
        }

        let Self { driver, registered, stats, .. } = self;
        let mut latest_slot = 0;
        adapter.consume(|top_bid: TopBidUpdate, _producers| {
            latest_slot = latest_slot.max(top_bid.slot);
            stats.updates_received += 1;
            for &token in registered.iter() {
                driver.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                    top_bid.ssz_append(buffer);
                });
                stats.sends += 1;
            }
        });

        if latest_slot > self.bid_slot {
            self.report_slot_stats();
            self.bid_slot = latest_slot;
        }
    }
}
