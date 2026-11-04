//! Builder side of the block-merging TCP protocol: a flux tile that accepts
//! relay connections (the relay dials, see the relay's `BlockMergingTile`),
//! runs the registration handshake, routes decoded control messages to the
//! merge engine and streams engine outputs back to the relay.

pub mod codec;
pub mod session;
#[cfg(test)]
mod tests;

use std::{
    cell::RefCell,
    ops::Range,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use codec::{append_body_frame, append_frame, decompress};
use flux::{
    spine::{
        DCacheMsg, DCacheRead, SpineAdapter, SpineDCacheConsumer, SpineProducer,
        SpineProducerWithDCache, SpineProducers,
    },
    tile::Tile,
    timing::Repeater,
};
use flux_network::{Event, Network, TcpGroupConfig, Token};
use helix_tcp_types::{
    Status,
    merging::{
        MERGING_HEADER_SIZE, MergingFrameHeader, MergingHeaderError, MergingMsgId,
        builder_to_relay::{FatalV1, RejectCode, RejectSubject, RejectV1},
        control::{MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1},
        relay_to_builder::{SlotEndV1, SlotStartV1},
    },
};
use rustc_hash::FxHashMap;
use session::{Session, SessionState, api_key_allowed, negotiate_version};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};

use crate::{
    config::MergingConfig,
    metrics,
    spine::{
        BuilderSpine, BuilderSpineProducers, EngineCursor, EventKind, FromEngine, OutputKind,
        ToEngine, charge,
    },
    utils::utcnow_ns,
};

const PING_INTERVAL_S: u64 = 5;

/// `MergedBlockV1.response_id`: the fixed field after `slot: u64`.
const MERGED_RESPONSE_ID: Range<usize> = 8..12;

/// `to_engine` events and bytes a mergeable block must leave free, so control events (a few
/// hundred bytes each) still fit while blocks are refused.
const CONTROL_HEADROOM: (u64, u64) = (16, 1 << 20);

/// Server side of `to_engine` flow control, see [`EngineCursor`].
struct EngineTx {
    cursor: Arc<EngineCursor>,
    seq: u64,
    charge: u64,
    /// Queue slots and dcache bytes, read from the producer on attach; until then every event is
    /// refused.
    depth: u64,
    ring: u64,
}

impl EngineTx {
    /// Publishes unless the event, plus `headroom` (events, bytes), would not fit in flight.
    fn send(
        &mut self,
        out: &BuilderSpineProducers,
        kind: EventKind,
        generation: u64,
        slot: u64,
        body: &[u8],
        headroom: (u64, u64),
    ) -> bool {
        let charge = if body.is_empty() { 0 } else { charge(body.len()) };
        let events = self.seq - self.cursor.seq.load(Ordering::Acquire);
        let bytes = self.charge - self.cursor.charge.load(Ordering::Acquire);
        if events + 1 + headroom.0 > self.depth || bytes + charge + headroom.1 > self.ring {
            return false;
        }
        let event = ToEngine {
            kind,
            _pad: [0; 7],
            generation,
            slot,
            recv_ns: utcnow_ns(),
            seq: self.seq + 1,
            charge: self.charge + charge,
        };
        let write = |buf: &mut [u8]| buf.copy_from_slice(body);
        let payload = (!body.is_empty()).then_some((body.len(), write));
        // Fails only for a payload over the ring, which the bytes check rules out.
        if out.produce_with_dcache(event, payload).is_err() {
            return false;
        }
        self.seq = event.seq;
        self.charge = event.charge;
        true
    }

    /// Fails only with the engine stalled past the queue; the event is dropped and the engine
    /// resyncs at the next `SlotStart` or generation.
    fn send_control(
        &mut self,
        out: &BuilderSpineProducers,
        kind: EventKind,
        generation: u64,
        slot: u64,
        body: &[u8],
    ) {
        if !self.send(out, kind, generation, slot, body, (0, 0)) {
            metrics::queue_drop("engine_control");
            error!(?kind, len = body.len(), "engine queue full, control event dropped");
        }
    }
}

/// A reply buffered during `poll_with` (the connector is exclusively borrowed
/// there) and written right after.
enum Reply {
    Ack(MergerAckV1),
    Pong(PongV1),
    Reject(RejectV1),
    Fatal(FatalV1),
}

pub struct MergingServerTile {
    listener: Network,

    api_keys: Vec<[u8; 16]>,
    max_orders_per_slot: u32,
    max_frame_bytes: u32,
    supports_zstd: bool,
    handshake_timeout: Duration,
    idle_disconnect: Duration,

    sessions: FxHashMap<Token, Session>,
    /// The registered relay connection; a newly registered one replaces it.
    active: Option<Token>,
    /// Bumped on every active-connection change; stale engine outputs are dropped.
    generation: u64,
    engine: EngineTx,

    ping: Repeater,
    ping_nonce: u64,

    // Buffered reactions, drained after poll_with.
    replies: Vec<(Token, Reply)>,
    to_disconnect: Vec<Token>,
    encode_scratch: Vec<u8>,
    /// The engine output being sent, copied out of the dcache.
    output_body: Vec<u8>,
    output_frame: Vec<u8>,
}

impl MergingServerTile {
    pub fn new(config: &MergingConfig, cursor: Arc<EngineCursor>) -> Self {
        // Telemetry disabled: per-connection shm queue leak, see the note in
        // the relay's tcp_bid_recv.
        let mut listener = Network::default();
        let group = listener.add_group(TcpGroupConfig {
            socket_buf_size: Some(config.socket_buf_size),
            max_frame_size: u32::MAX as usize,
            ..Default::default()
        });
        listener
            .listen(group, config.listen_addr)
            .expect("failed to initialise the merging TCP listener");
        info!(listen_addr = %config.listen_addr, "merging server listening");

        Self {
            listener,
            api_keys: config.api_keys.iter().map(|key| key.into_bytes()).collect(),
            max_orders_per_slot: config.max_orders_per_slot,
            max_frame_bytes: config.max_frame_bytes,
            supports_zstd: config.supports_zstd,
            handshake_timeout: Duration::from_millis(config.handshake_timeout_ms),
            idle_disconnect: Duration::from_secs(config.idle_disconnect_s),
            sessions: FxHashMap::default(),
            active: None,
            generation: 0,
            engine: EngineTx { cursor, seq: 0, charge: 0, depth: 0, ring: 0 },
            ping: Repeater::every(flux::timing::Duration::from_secs(PING_INTERVAL_S)),
            ping_nonce: 0,
            replies: Vec::new(),
            to_disconnect: Vec::new(),
            encode_scratch: Vec::new(),
            output_body: Vec::new(),
            output_frame: Vec::new(),
        }
    }

    fn drain_engine_outputs(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        let body = RefCell::new(std::mem::take(&mut self.output_body));
        adapter.consume_with_dcache(
            |_, payload: &[u8]| {
                let mut body = body.borrow_mut();
                body.clear();
                body.extend_from_slice(payload);
            },
            |read, _| match read {
                DCacheRead::Ok((output, ())) => self.send_output(output, &mut body.borrow_mut()),
                DCacheRead::NoRef(_) | DCacheRead::Lost(_) | DCacheRead::SpedPast => {
                    metrics::queue_drop("engine_output_lost");
                    warn!("engine output lost");
                }
            },
        );
        self.output_body = body.into_inner();
    }

    fn send_output(&mut self, output: FromEngine, body: &mut [u8]) {
        if output.generation != self.generation {
            trace!("dropping stale engine output");
            return;
        }
        let Some(token) = self.active else { return };
        let Some(session) = self.sessions.get_mut(&token) else { return };
        let frame = &mut self.output_frame;
        frame.clear();
        match output.kind {
            OutputKind::Merged => {
                let response_id = session.next_response_id();
                body[MERGED_RESPONSE_ID].copy_from_slice(&response_id.to_le_bytes());
                let slot = u64::from_le_bytes(body[..8].try_into().expect("8 bytes"));
                debug!(slot, response_id, len = body.len(), "sending merged block");
                append_body_frame(frame, MergingMsgId::MergedBlockV1, body, session.zstd());
            }
            OutputKind::Reject => {
                debug!(len = body.len(), "sending reject");
                append_body_frame(frame, MergingMsgId::RejectV1, body, false);
            }
        }
        self.listener.send_with(token, |buf| buf.extend_from_slice(frame));
    }

    fn poll_sockets(&mut self, out: &BuilderSpineProducers) {
        let Self {
            listener,
            api_keys,
            max_orders_per_slot,
            max_frame_bytes,
            supports_zstd,
            handshake_timeout,
            sessions,
            active,
            generation,
            engine,
            replies,
            to_disconnect,
            ..
        } = self;

        listener.poll_with(|event| match event {
            Event::Accepted { token: stream, peer_addr, .. } => {
                info!(?peer_addr, token = ?stream, "relay connected, awaiting registration");
                sessions.insert(stream, Session::awaiting(Instant::now(), *handshake_timeout));
            }
            Event::Connected { token, .. } => {
                // Outbound-connection event; a listener should only ever see
                // fresh accepts.
                warn!(?token, "unexpected connected event on merging listener");
            }
            Event::Disconnected { token, .. } => {
                info!(?token, "relay disconnected");
                sessions.remove(&token);
                if *active == Some(token) {
                    *active = None;
                    *generation += 1;
                    engine.send_control(out, EventKind::ConnectionReset, *generation, 0, &[]);
                }
            }
            Event::Message { token, payload, .. } => {
                let Some(session) = sessions.get_mut(&token) else {
                    warn!(?token, "message from unknown session");
                    to_disconnect.push(token);
                    return;
                };
                session.last_recv = Instant::now();

                match &session.state {
                    SessionState::AwaitingRegistration { .. } => {
                        if let Some(reply) = handle_registration(
                            token,
                            payload,
                            api_keys,
                            *supports_zstd,
                            *max_orders_per_slot,
                            *max_frame_bytes,
                            sessions,
                            active,
                            generation,
                            out,
                            engine,
                            to_disconnect,
                        ) {
                            replies.push((token, reply));
                        }
                    }
                    SessionState::Active { .. } => handle_active_message(
                        token,
                        payload,
                        session_zstd(sessions, token),
                        *max_frame_bytes,
                        *generation,
                        out,
                        engine,
                        replies,
                        to_disconnect,
                    ),
                }
            }
        });

        self.flush_reactions(out);
    }

    fn flush_reactions(&mut self, out: &BuilderSpineProducers) {
        for (token, reply) in self.replies.drain(..) {
            match &reply {
                Reply::Reject(msg) => metrics::reject_sent(reject_label(msg.code), false),
                Reply::Fatal(msg) => metrics::reject_sent(reject_label(msg.code), true),
                Reply::Ack(_) | Reply::Pong(_) => {}
            }
            let buf = &mut self.encode_scratch;
            buf.clear();
            match &reply {
                Reply::Ack(msg) => append_frame(buf, MergingMsgId::MergerAckV1, msg),
                Reply::Pong(msg) => append_frame(buf, MergingMsgId::PongV1, msg),
                Reply::Reject(msg) => append_frame(buf, MergingMsgId::RejectV1, msg),
                Reply::Fatal(msg) => append_frame(buf, MergingMsgId::FatalV1, msg),
            }
            self.listener.send_with(token, |buf| buf.extend_from_slice(&self.encode_scratch));
        }
        metrics::relay_connected(self.active.is_some());
        for token in std::mem::take(&mut self.to_disconnect) {
            self.listener.disconnect(token);
            self.sessions.remove(&token);
            if self.active == Some(token) {
                self.active = None;
                self.generation += 1;
                self.engine.send_control(out, EventKind::ConnectionReset, self.generation, 0, &[]);
            }
        }
    }

    /// Ping the active connection and enforce handshake/idle deadlines.
    fn tick_timers(&mut self, out: &BuilderSpineProducers) {
        if !self.ping.fired() {
            return;
        }

        if let Some(token) = self.active &&
            self.sessions.contains_key(&token)
        {
            self.ping_nonce += 1;
            let msg = PingV1 { nonce: self.ping_nonce };
            self.encode_scratch.clear();
            append_frame(&mut self.encode_scratch, MergingMsgId::PingV1, &msg);
            self.listener.send_with(token, |buf| buf.extend_from_slice(&self.encode_scratch));
        }

        let now = Instant::now();
        for (token, session) in self.sessions.iter() {
            let expired = match &session.state {
                SessionState::AwaitingRegistration { deadline } => {
                    if now > *deadline {
                        warn!(?token, "registration handshake timed out");
                        true
                    } else {
                        false
                    }
                }
                SessionState::Active { .. } => {
                    if now.duration_since(session.last_recv) > self.idle_disconnect {
                        warn!(?token, "idle relay connection, disconnecting");
                        true
                    } else {
                        false
                    }
                }
            };
            if expired {
                self.to_disconnect.push(*token);
            }
        }
        self.flush_reactions(out);
    }
}

impl MergingServerTile {
    /// One tile iteration; tests drive it with their own adapter.
    pub fn run_once(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        self.drain_engine_outputs(adapter);
        self.poll_sockets(&adapter.producers);
        self.tick_timers(&adapter.producers);
    }
}

impl Tile<BuilderSpine> for MergingServerTile {
    fn on_attach(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        let outputs: &mut SpineDCacheConsumer<FromEngine> = adapter.consumers.as_mut();
        outputs.inner.subscribe_broadcast();
        let events: &SpineProducerWithDCache<ToEngine> = adapter.producers.as_ref();
        self.engine.ring = events.dcache_ptr().capacity() as u64;
        let queue: &SpineProducer<DCacheMsg<ToEngine>> = events.as_ref();
        self.engine.depth = queue.queue.n_slots() as u64;
        info!(depth = self.engine.depth, ring = self.engine.ring, "engine queue");
    }

    fn loop_body(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        self.run_once(adapter);
    }

    fn name(&self) -> flux::tile::TileName {
        flux_utils::short_typename::<Self>()
    }
}

fn session_zstd(sessions: &FxHashMap<Token, Session>, token: Token) -> bool {
    sessions.get(&token).is_some_and(|s| s.zstd())
}

fn reject_label(code: RejectCode) -> &'static str {
    match code {
        RejectCode::HeadMismatch => "head_mismatch",
        RejectCode::NotSynced => "not_synced",
        RejectCode::UnknownBaseBlock => "unknown_base_block",
        RejectCode::InvalidPayment => "invalid_payment",
        RejectCode::UnknownCollateral => "unknown_collateral",
        RejectCode::InvalidOrder => "invalid_order",
        RejectCode::StaleSlot => "stale_slot",
        RejectCode::LimitExceeded => "limit_exceeded",
        RejectCode::Busy => "busy",
        RejectCode::InvalidBaseBlock => "invalid_base_block",
    }
}

#[expect(clippy::too_many_arguments)]
fn handle_registration(
    token: Token,
    payload: &[u8],
    api_keys: &[[u8; 16]],
    supports_zstd: bool,
    max_orders_per_slot: u32,
    max_frame_bytes: u32,
    sessions: &mut FxHashMap<Token, Session>,
    active: &mut Option<Token>,
    generation: &mut u64,
    out: &BuilderSpineProducers,
    engine: &mut EngineTx,
    to_disconnect: &mut Vec<Token>,
) -> Option<Reply> {
    let err_ack = |msg: &str| MergerAckV1 {
        version: 0,
        status: Status::InvalidRequest,
        error_msg: msg.as_bytes().to_vec(),
        max_orders_per_slot: 0,
        max_frame_bytes: 0,
    };

    let header = match MergingFrameHeader::decode(payload) {
        Ok(header) => header,
        Err(err) => {
            warn!(?token, %err, "bad frame during registration, disconnecting");
            to_disconnect.push(token);
            return None;
        }
    };
    if header.msg_id != MergingMsgId::MergerRegistrationV1 || header.is_zstd_compressed() {
        warn!(?token, msg_id = ?header.msg_id, "expected registration frame, disconnecting");
        to_disconnect.push(token);
        return None;
    }
    let Ok(reg) = MergerRegistrationV1::from_ssz_bytes(&payload[MERGING_HEADER_SIZE..]) else {
        warn!(?token, "undecodable registration, disconnecting");
        to_disconnect.push(token);
        return None;
    };

    if !api_key_allowed(api_keys, &reg.api_key) {
        warn!(?token, "unknown api key, disconnecting");
        to_disconnect.push(token);
        return Some(Reply::Ack(err_ack("unknown api key")));
    }
    let Some(version) = negotiate_version(reg.min_version, reg.max_version) else {
        warn!(
            ?token,
            min = reg.min_version,
            max = reg.max_version,
            "no protocol version overlap, disconnecting"
        );
        to_disconnect.push(token);
        return Some(Reply::Ack(err_ack("unsupported protocol version")));
    };

    let relay_id = String::from_utf8_lossy(&reg.relay_id).into_owned();
    let zstd = reg.supports_zstd && supports_zstd;
    info!(?token, relay_id, version, zstd, "relay registered");

    if let Some(session) = sessions.get_mut(&token) {
        session.state = SessionState::Active { zstd };
    }

    // Single-connection policy: the newest registered connection wins.
    if let Some(old) = active.replace(token) &&
        old != token
    {
        warn!(?old, new = ?token, "replacing existing relay connection");
        to_disconnect.push(old);
        // Removed here so flush doesn't treat it as the active one.
        sessions.remove(&old);
    }
    *generation += 1;
    engine.send_control(out, EventKind::ConnectionReset, *generation, 0, &[]);

    Some(Reply::Ack(MergerAckV1 {
        version,
        status: Status::Okay,
        error_msg: Vec::new(),
        max_orders_per_slot,
        max_frame_bytes,
    }))
}

#[expect(clippy::too_many_arguments)]
fn handle_active_message(
    token: Token,
    payload: &[u8],
    zstd_negotiated: bool,
    max_frame_bytes: u32,
    generation: u64,
    out: &BuilderSpineProducers,
    engine: &mut EngineTx,
    replies: &mut Vec<(Token, Reply)>,
    to_disconnect: &mut Vec<Token>,
) {
    let fatal = |replies: &mut Vec<(Token, Reply)>,
                 to_disconnect: &mut Vec<Token>,
                 code: RejectCode,
                 msg: &str| {
        replies.push((token, Reply::Fatal(FatalV1 { code, msg: msg.as_bytes().to_vec() })));
        to_disconnect.push(token);
    };

    let header = match MergingFrameHeader::decode(payload) {
        Ok(header) => header,
        // Extension ids must be ignored.
        Err(MergingHeaderError::ExtensionMsgId(_)) => return,
        Err(err) => {
            warn!(?token, %err, "bad merging frame, disconnecting");
            fatal(replies, to_disconnect, RejectCode::InvalidOrder, "bad frame header");
            return;
        }
    };

    let raw_body = &payload[MERGING_HEADER_SIZE..];
    let decompressed;
    let body: &[u8] = if header.is_zstd_compressed() {
        if !zstd_negotiated {
            warn!(?token, "zstd frame but compression was not negotiated");
            fatal(replies, to_disconnect, RejectCode::InvalidOrder, "zstd not negotiated");
            return;
        }
        match decompress(raw_body, max_frame_bytes as usize) {
            Ok(data) => {
                decompressed = data;
                &decompressed
            }
            Err(err) => {
                warn!(?token, %err, "zstd decompression failed");
                fatal(
                    replies,
                    to_disconnect,
                    RejectCode::InvalidOrder,
                    "zstd decompression failed",
                );
                return;
            }
        }
    } else {
        raw_body
    };

    match header.msg_id {
        MergingMsgId::PingV1 => {
            if let Ok(ping) = PingV1::from_ssz_bytes(body) {
                replies.push((token, Reply::Pong(PongV1 { nonce: ping.nonce })));
            }
        }
        MergingMsgId::PongV1 => {
            if let Ok(pong) = PongV1::from_ssz_bytes(body) {
                trace!(?token, nonce = pong.nonce, "merging pong");
            }
        }
        MergingMsgId::RelayConfigV1 => {
            let Ok(config) = RelayConfigV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable relay config");
                return;
            };
            if let Err(err) = config.validate() {
                warn!(?token, %err, "invalid relay config");
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "invalid relay config");
                return;
            }
            metrics::relay_frame("relay_config");
            engine.send_control(out, EventKind::RelayConfig, generation, 0, body);
        }
        MergingMsgId::SlotStartV1 => {
            let Ok(msg) = SlotStartV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable slot start");
                return;
            };
            debug!(slot = msg.slot, "slot start");
            metrics::relay_frame("slot_start");
            engine.send_control(out, EventKind::SlotStart, generation, msg.slot, body);
        }
        MergingMsgId::SlotEndV1 => {
            let Ok(msg) = SlotEndV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable slot end");
                return;
            };
            debug!(slot = msg.slot, "slot end");
            metrics::relay_frame("slot_end");
            engine.send_control(out, EventKind::SlotEnd, generation, msg.slot, &[]);
        }
        MergingMsgId::ActivateBaseBlockV1 => {}
        MergingMsgId::MergeableBlockV1 => {
            metrics::relay_frame("mergeable_block");
            let slot = peek_slot(body).unwrap_or_default();
            // Decompression is capped at max_frame_bytes; an uncompressed frame is not.
            let (code, msg): (_, &[u8]) = if body.len() > max_frame_bytes as usize {
                (RejectCode::LimitExceeded, b"block exceeds max_frame_bytes")
            } else if engine.send(
                out,
                EventKind::MergeableBlock,
                generation,
                slot,
                body,
                CONTROL_HEADROOM,
            ) {
                return;
            } else {
                metrics::queue_drop("engine_mergeable");
                (RejectCode::Busy, b"engine queue full")
            };
            debug!(?token, slot, len = body.len(), ?code, "rejecting mergeable block");
            let subject = RejectSubject::None(0);
            replies
                .push((token, Reply::Reject(RejectV1 { slot, code, subject, msg: msg.to_vec() })));
        }
        other => {
            warn!(?token, ?other, "unexpected merging msg from relay, disconnecting");
            fatal(replies, to_disconnect, RejectCode::InvalidOrder, "unexpected message id");
        }
    }
}

/// First SSZ field of `MergeableBlockV1` is `slot: u64`.
fn peek_slot(body: &[u8]) -> Option<u64> {
    body.get(..8).map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
}
