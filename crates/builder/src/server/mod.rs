//! Builder side of the block-merging TCP protocol: a flux tile that accepts
//! relay connections (the relay dials, see the relay's `BlockMergingTile`),
//! runs the registration handshake, routes decoded control messages to the
//! merge engine and streams engine outputs back to the relay.

pub mod codec;
pub mod session;
#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use codec::{append_frame, append_frame_maybe_zstd, decompress};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use flux::{tile::Tile, timing::Repeater};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use helix_tcp_types::{
    Status,
    merging::{
        MERGING_HEADER_SIZE, MergingFrameHeader, MergingHeaderError, MergingMsgId,
        builder_to_relay::{FatalV1, RejectCode, RejectSubject, RejectV1},
        control::{MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1},
        relay_to_builder::{ActivateBaseBlockV1, SlotEndV1, SlotStartV1},
    },
};
use rustc_hash::FxHashMap;
use session::{Session, SessionState, api_key_allowed, negotiate_version};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};

use crate::{
    config::MergingConfig,
    engine::{EngineEvent, EngineOutput},
    spine::BuilderSpine,
    utils::utcnow_ns,
};

const PING_INTERVAL_S: u64 = 5;

/// A reply buffered during `poll_with` (the connector is exclusively borrowed
/// there) and written right after.
enum Reply {
    Ack(MergerAckV1),
    Pong(PongV1),
    Reject(RejectV1),
    Fatal(FatalV1),
}

pub struct MergingServerTile {
    listener: TcpConnector,

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

    engine_tx: Sender<EngineEvent>,
    engine_rx: Receiver<EngineOutput>,

    ping: Repeater,
    ping_nonce: u64,

    // Buffered reactions, drained after poll_with.
    replies: Vec<(Token, Reply)>,
    to_disconnect: Vec<Token>,
    encode_scratch: Vec<u8>,
}

impl MergingServerTile {
    pub fn new(
        config: &MergingConfig,
        engine_tx: Sender<EngineEvent>,
        engine_rx: Receiver<EngineOutput>,
    ) -> Self {
        // Telemetry disabled: per-connection shm queue leak, see the note in
        // the relay's tcp_bid_recv.
        let mut listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Disabled)
            .with_socket_buf_size(config.socket_buf_size);
        listener
            .listen_at(config.listen_addr)
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
            engine_tx,
            engine_rx,
            ping: Repeater::every(flux::timing::Duration::from_secs(PING_INTERVAL_S)),
            ping_nonce: 0,
            replies: Vec::new(),
            to_disconnect: Vec::new(),
            encode_scratch: Vec::new(),
        }
    }

    fn drain_engine_outputs(&mut self) {
        while let Ok(output) = self.engine_rx.try_recv() {
            if output.generation() != self.generation {
                trace!("dropping stale engine output");
                continue;
            }
            let Some(token) = self.active else { continue };
            let Some(session) = self.sessions.get_mut(&token) else { continue };
            let zstd = session.zstd();
            match output {
                EngineOutput::Merged { mut msg, .. } => {
                    msg.response_id = session.next_response_id();
                    debug!(
                        slot = msg.slot,
                        response_id = msg.response_id,
                        base_block_hash = %msg.base_block_hash,
                        proposer_value = %msg.proposer_value,
                        "sending merged block"
                    );
                    let mut frame = Vec::new();
                    append_frame_maybe_zstd(
                        &mut frame,
                        MergingMsgId::MergedBlockV1,
                        msg.as_ref(),
                        zstd,
                        &mut self.encode_scratch,
                    );
                    self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                        buf.extend_from_slice(&frame);
                    });
                }
                EngineOutput::Reject { msg, .. } => {
                    debug!(slot = msg.slot, code = ?msg.code, subject = ?msg.subject, "sending reject");
                    self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                        append_frame(buf, MergingMsgId::RejectV1, &msg);
                    });
                }
            }
        }
    }

    fn poll_sockets(&mut self) {
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
            engine_tx,
            replies,
            to_disconnect,
            ..
        } = self;

        listener.poll_with(|event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                info!(?peer_addr, token = ?stream, "relay connected, awaiting registration");
                sessions.insert(stream, Session::awaiting(Instant::now(), *handshake_timeout));
            }
            PollEvent::Reconnect { token } => {
                // Reconnects are an outbound-connection feature; a listener
                // should only ever see fresh accepts.
                warn!(?token, "unexpected reconnect event on merging listener");
            }
            PollEvent::Disconnect { token } => {
                info!(?token, "relay disconnected");
                sessions.remove(&token);
                if *active == Some(token) {
                    *active = None;
                    *generation += 1;
                    send_control(engine_tx, EngineEvent::ConnectionReset {
                        generation: *generation,
                    });
                }
            }
            PollEvent::Message { token, payload, send_ts: _ } => {
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
                            engine_tx,
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
                        engine_tx,
                        replies,
                        to_disconnect,
                    ),
                }
            }
        });

        self.flush_reactions();
    }

    fn flush_reactions(&mut self) {
        for (token, reply) in self.replies.drain(..) {
            self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buf| match &reply {
                Reply::Ack(msg) => append_frame(buf, MergingMsgId::MergerAckV1, msg),
                Reply::Pong(msg) => append_frame(buf, MergingMsgId::PongV1, msg),
                Reply::Reject(msg) => append_frame(buf, MergingMsgId::RejectV1, msg),
                Reply::Fatal(msg) => append_frame(buf, MergingMsgId::FatalV1, msg),
            });
        }
        for token in std::mem::take(&mut self.to_disconnect) {
            self.listener.disconnect(token);
            self.sessions.remove(&token);
            if self.active == Some(token) {
                self.active = None;
                self.generation += 1;
                send_control(&self.engine_tx, EngineEvent::ConnectionReset {
                    generation: self.generation,
                });
            }
        }
    }

    /// Ping the active connection and enforce handshake/idle deadlines.
    fn tick_timers(&mut self) {
        if !self.ping.fired() {
            return;
        }

        if let Some(token) = self.active &&
            self.sessions.contains_key(&token)
        {
            self.ping_nonce += 1;
            let msg = PingV1 { nonce: self.ping_nonce };
            self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                append_frame(buf, MergingMsgId::PingV1, &msg);
            });
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
        self.flush_reactions();
    }
}

impl MergingServerTile {
    /// One tile iteration; also drivable without a spine (tests).
    pub fn run_once(&mut self) {
        self.drain_engine_outputs();
        self.poll_sockets();
        self.tick_timers();
    }
}

impl Tile<BuilderSpine> for MergingServerTile {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<BuilderSpine>) {
        self.run_once();
    }

    fn name(&self) -> flux::tile::TileName {
        flux_utils::short_typename::<Self>()
    }
}

fn session_zstd(sessions: &FxHashMap<Token, Session>, token: Token) -> bool {
    sessions.get(&token).is_some_and(|s| s.zstd())
}

/// Control events must not be lost; a full queue here means the engine is
/// wedged, which is fatal for the merge pipeline (the TCP tile keeps serving).
fn send_control(engine_tx: &Sender<EngineEvent>, event: EngineEvent) {
    if let Err(TrySendError::Full(_)) = engine_tx.try_send(event) {
        error!("engine event queue full, dropping control event");
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
    engine_tx: &Sender<EngineEvent>,
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
    send_control(engine_tx, EngineEvent::ConnectionReset { generation: *generation });

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
    engine_tx: &Sender<EngineEvent>,
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
            send_control(engine_tx, EngineEvent::RelayConfig(config));
        }
        MergingMsgId::SlotStartV1 => {
            let Ok(msg) = SlotStartV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable slot start");
                return;
            };
            debug!(slot = msg.slot, "slot start");
            send_control(engine_tx, EngineEvent::SlotStart(msg));
        }
        MergingMsgId::SlotEndV1 => {
            let Ok(msg) = SlotEndV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable slot end");
                return;
            };
            debug!(slot = msg.slot, "slot end");
            send_control(engine_tx, EngineEvent::SlotEnd { slot: msg.slot });
        }
        MergingMsgId::ActivateBaseBlockV1 => {
            let Ok(msg) = ActivateBaseBlockV1::from_ssz_bytes(body) else {
                fatal(replies, to_disconnect, RejectCode::InvalidOrder, "undecodable activation");
                return;
            };
            debug!(slot = msg.slot, block_hash = %msg.block_hash, "activate base block");
            send_control(engine_tx, EngineEvent::ActivateBase {
                slot: msg.slot,
                block_hash: msg.block_hash,
                recv_ns: utcnow_ns(),
                generation,
            });
        }
        MergingMsgId::MergeableBlockV1 => {
            // Not decoded here: the engine does SSZ + tx decoding off the tile
            // thread. Only the slot for a possible Busy reject is peeked.
            let event = EngineEvent::MergeableBlock {
                body: body.to_vec(),
                recv_ns: utcnow_ns(),
                generation,
            };
            if let Err(TrySendError::Full(_)) = engine_tx.try_send(event) {
                let slot = peek_slot(body).unwrap_or_default();
                debug!(?token, slot, "engine busy, rejecting mergeable block");
                replies.push((
                    token,
                    Reply::Reject(RejectV1 {
                        slot,
                        code: RejectCode::Busy,
                        subject: RejectSubject::None(0),
                        msg: b"engine queue full".to_vec(),
                    }),
                ));
            }
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
