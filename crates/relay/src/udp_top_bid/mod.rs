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

/// Top bids are a latest-value feed, so the transport is unreliable: a lost
/// update is superseded by the next one rather than resent. Datagrams leave
/// the window once the kernel takes them; a peer whose socket stays blocked
/// past the backlog is cut.
const SEND_WINDOW: usize = 64;
const MAX_BACKLOG: usize = 64;
const MAX_BACKLOG_TIMEOUT_MS: u64 = 100;
/// A `TopBidUpdate` is 188 bytes and a `RegistrationMsg` is 64; the cap only
/// has to stay under `SEND_WINDOW` datagrams, which flux asserts.
const MAX_MESSAGE_SIZE: usize = 4096;

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
    max_per_key: usize,

    /// Authenticated peers, the only tokens an update is written to.
    registered: Vec<(Token, [u8; 16])>,
    /// Accepted but not yet registered, with their accept time.
    pending: Vec<(Token, Instant)>,
    to_disconnect: Vec<Token>,

    send_buf_scratch: Vec<u8>,
    registration_timeout: Duration,

    bid_slot: u64,
    stats: Stats,
}

impl UdpTopBidTile {
    pub fn new(
        listener_addr: SocketAddr,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_per_key: usize,
    ) -> Self {
        let udp = UdpConfig {
            send_window: SEND_WINDOW,
            max_message_size: MAX_MESSAGE_SIZE,
            reliable: false,
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
            max_per_key,
            registered: Vec::with_capacity(256),
            pending: Vec::with_capacity(256),
            to_disconnect: Vec::with_capacity(256),
            send_buf_scratch: Vec::with_capacity(MAX_MESSAGE_SIZE),
            registration_timeout: Duration::from_millis(REGISTRATION_TIMEOUT_MS),
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

fn can_connect(
    cache: &DashMap<String, Vec<BlsPublicKeyBytes>>,
    msg: &RegistrationMsg,
    api_key: &str,
    registered: &[(Token, [u8; 16])],
    max_per_key: usize,
) -> bool {
    // api-key is allowed to connect via udp
    let is_authorised = cache.get(api_key).is_some_and(|p| p.value().contains(&msg.builder_pubkey)) ||
        (is_local_dev() && cache.contains_key(api_key));
    if !is_authorised {
        return false;
    }

    // api-key hasn't reached it's max conns
    // registered.len() is expected to be low.
    registered.iter().filter(|(_, k)| *k == msg.api_key).count() < max_per_key
}

impl Tile<HelixSpine> for UdpTopBidTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        let Self {
            driver,
            api_key_cache,
            max_per_key,
            registered,
            pending,
            to_disconnect,
            stats,
            ..
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
                if let Some(i) = registered.iter().position(|(t, _)| *t == token) {
                    registered.swap_remove(i);
                } else if let Some(i) = pending.iter().position(|(t, _)| *t == token) {
                    pending.swap_remove(i);
                }
            }
            PollEvent::Message { token, payload, send_ts: _ } => {
                let Some(pending_bid_id) = pending.iter().position(|(t, _)| *t == token) else {
                    // Registered peers have nothing to say on this feed.
                    return;
                };
                pending.swap_remove(pending_bid_id);
                let msg = match RegistrationMsg::from_ssz_bytes(payload) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::error!(err=?e, "invalid udp registration message");
                        stats.registration_invalid += 1;
                        to_disconnect.push(token);
                        return;
                    }
                };

                let mut key_buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
                let api_key =
                    Uuid::from_bytes(msg.api_key).as_hyphenated().encode_lower(&mut key_buf);
                if can_connect(api_key_cache, &msg, api_key, registered, *max_per_key) {
                    stats.registration_ok += 1;
                    registered.push((token, msg.api_key));
                } else {
                    stats.registration_invalid += 1;
                    to_disconnect.push(token);
                }
            }
        });

        let Self { pending, to_disconnect, stats, registration_timeout, .. } = self;
        pending.retain(|(token, accepted_at)| {
            if accepted_at.elapsed() <= *registration_timeout {
                return true;
            }
            stats.registration_timeouts += 1;
            to_disconnect.push(*token);
            false
        });

        for token in self.to_disconnect.drain(..) {
            self.driver.disconnect(token);
        }

        let Self { driver, registered, send_buf_scratch: send_buf, stats, .. } = self;
        let mut latest_slot = 0;
        adapter.consume(|top_bid: TopBidUpdate, _producers| {
            latest_slot = latest_slot.max(top_bid.slot);
            stats.updates_received += 1;
            if registered.is_empty() {
                return;
            }
            send_buf.clear();
            top_bid.ssz_append(send_buf);
            let payload: &[u8] = send_buf;
            for &(token, _) in registered.iter() {
                driver.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                    buffer.extend_from_slice(payload);
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
