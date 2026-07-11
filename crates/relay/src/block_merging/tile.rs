use std::sync::Arc;

use alloy_primitives::{B256, U256};
use flux::{
    spine::SpineProducers,
    tile::Tile,
    timing::{Duration, Repeater},
};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use flux_utils::SharedVector;
use helix_common::{BlockMergingTcpConfig, api::builder_api::TopBidUpdate};
use helix_tcp_types::merging::{
    MERGING_HEADER_SIZE, MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingHeaderError,
    MergingMsgId,
    builder_to_relay::{FatalV1, MergedBlockV1, RejectV1},
    control::{
        BuilderCollateral, MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1,
    },
    relay_to_builder::{ActivateBaseBlockV1, MergeableBlockV1, SlotStartV1},
};
use helix_types::{Submission, payload_to_v3};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::{
    HelixSpine, SubmissionDataWithSpan,
    block_merging::{append_frame, merged_block_to_response, order_to_ref},
    housekeeper::SlotUpdate,
    simulator::BlockMergeResponse,
    spine::messages::{DecodedSubmission, MergedBlockMsg, SlotMsg},
};

const REDIAL_INTERVAL_S: u64 = 2;
const PING_INTERVAL_S: u64 = 5;

struct Endpoint {
    addr: std::net::SocketAddr,
    api_key: [u8; 16],
    token: Option<Token>,
}

#[derive(Default)]
struct Conn {
    endpoint_ix: usize,
    /// Handshake completed (ack received with ok status).
    active: bool,
    max_orders_per_slot: u32,
    max_frame_bytes: u32,
    orders_sent: u32,
    /// Appendable block hashes forwarded on this connection this slot.
    forwarded: FxHashSet<B256>,
    activated: Option<B256>,
}

impl Conn {
    fn reset(&mut self) {
        self.active = false;
        self.reset_slot();
    }

    fn reset_slot(&mut self) {
        self.orders_sent = 0;
        self.forwarded.clear();
        self.activated = None;
    }
}

#[derive(Default)]
struct SlotState {
    bid_slot: u64,
    /// Set once registration + payload attributes arrived and the broadcast
    /// went out; cached for handshake replay.
    slot_start: Option<SlotStartV1>,
    fee_recipient: Option<alloy_primitives::Address>,
    /// parent_hash -> parent_beacon_block_root.
    attrs: FxHashMap<B256, B256>,
    /// Appendable block hashes forwarded this slot (any connection).
    appendable: FxHashSet<B256>,
    /// Decoded ixs forwarded this slot, replayed on re-handshake.
    mergeable_ixs: Vec<usize>,
    /// base_block_hash -> best proposer_value across all builders.
    best_merged: FxHashMap<B256, U256>,
}

pub struct BlockMergingTile {
    connector: TcpConnector,
    relay_id: Vec<u8>,
    relay_config_msg: RelayConfigV1,

    endpoints: Vec<Endpoint>,
    conns: FxHashMap<Token, Conn>,
    slot: SlotState,

    redial: Repeater,
    ping: Repeater,
    ping_nonce: u64,

    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    slot_events: Arc<SharedVector<SlotUpdate>>,
    merged_blocks: Arc<SharedVector<BlockMergeResponse>>,

    // Buffered during `poll_with` (the connector is exclusively borrowed
    // there), drained right after.
    to_disconnect: Vec<Token>,
    to_register: Vec<Token>,
    handshaken: Vec<Token>,
    pongs: Vec<(Token, u64)>,
    merged_ixs: Vec<usize>,
    encode_buf: Vec<u8>,
}

impl Tile<HelixSpine> for BlockMergingTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.poll_sockets();

        for ix in std::mem::take(&mut self.merged_ixs) {
            adapter.producers.produce(MergedBlockMsg { ix });
        }

        if self.redial.fired() {
            self.dial_endpoints();
        }
        if self.ping.fired() {
            self.send_pings();
        }

        adapter.consume(|msg: SlotMsg, _| self.on_slot_msg(msg));
        adapter.consume(|msg: DecodedSubmission, _| self.forward_decoded(msg.ix, None));
        adapter.consume(|top_bid: TopBidUpdate, _| self.on_top_bid(top_bid));
    }

    fn try_init(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) -> bool {
        self.dial_endpoints();
        info!("starting");
        true
    }

    fn name(&self) -> flux::tile::TileName {
        flux_utils::short_typename::<Self>()
    }
}

impl BlockMergingTile {
    pub fn new(
        config: BlockMergingTcpConfig,
        relay_id: String,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        slot_events: Arc<SharedVector<SlotUpdate>>,
        merged_blocks: Arc<SharedVector<BlockMergeResponse>>,
    ) -> Self {
        let relay_config_msg = RelayConfigV1 {
            relay_fee_recipient: config.relay_fee_recipient,
            multisend_contract: config.multisend_contract,
            relay_bps: config.relay_bps,
            merged_builder_bps: config.merged_builder_bps,
            winning_builder_bps: config.winning_builder_bps,
            distribution_gas_limit: config.distribution_gas_limit,
            builder_collaterals: config
                .builder_collaterals
                .iter()
                .map(|c| BuilderCollateral {
                    builder_coinbase: c.builder_coinbase,
                    collateral_safe: c.collateral_safe,
                })
                .collect(),
        };
        relay_config_msg.validate().expect("invalid block merging relay config");

        let endpoints = config
            .builders
            .iter()
            .map(|b| Endpoint {
                addr: b.addr,
                api_key: Uuid::parse_str(&b.api_key)
                    .expect("invalid block merging api key")
                    .into_bytes(),
                token: None,
            })
            .collect();

        // TODO: enable telemetry once the per-connection shm queue leak is fixed
        // Disabled: per-connection shm queue leak, see tcp_bid_recv/mod.rs.
        let connector = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Disabled)
            .with_socket_buf_size(64 * 1024 * 1024);

        Self {
            connector,
            relay_id: relay_id.into_bytes(),
            relay_config_msg,
            endpoints,
            conns: FxHashMap::default(),
            slot: SlotState::default(),
            redial: Repeater::every(Duration::from_secs(REDIAL_INTERVAL_S)),
            ping: Repeater::every(Duration::from_secs(PING_INTERVAL_S)),
            ping_nonce: 0,
            decoded,
            slot_events,
            merged_blocks,
            to_disconnect: Vec::new(),
            to_register: Vec::new(),
            handshaken: Vec::new(),
            pongs: Vec::new(),
            merged_ixs: Vec::new(),
            encode_buf: Vec::new(),
        }
    }

    /// Dials endpoints that have no token yet. A failed initial `connect` is
    /// not retried by the connector (unlike established conns, which
    /// auto-reconnect), so this runs on a repeater.
    fn dial_endpoints(&mut self) {
        for ix in 0..self.endpoints.len() {
            if self.endpoints[ix].token.is_some() {
                continue;
            }
            let addr = self.endpoints[ix].addr;
            let Some(token) = self.connector.connect(addr) else {
                warn!(%addr, "failed to dial merging builder");
                continue;
            };
            info!(%addr, ?token, "dialing merging builder");
            self.endpoints[ix].token = Some(token);
            self.conns.insert(token, Conn { endpoint_ix: ix, ..Default::default() });
            self.send_registration(token);
        }
    }

    fn send_registration(&mut self, token: Token) {
        let Some(conn) = self.conns.get(&token) else { return };
        let msg = MergerRegistrationV1 {
            api_key: self.endpoints[conn.endpoint_ix].api_key,
            relay_id: self.relay_id.clone(),
            min_version: MERGING_PROTOCOL_VERSION,
            max_version: MERGING_PROTOCOL_VERSION,
            supports_zstd: false,
        };
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            append_frame(buf, MergingMsgId::MergerRegistrationV1, &msg);
        });
    }

    /// Ack received: send the relay config, the current slot start and replay
    /// this slot's mergeable blocks.
    fn complete_handshake(&mut self, token: Token) {
        let msg = &self.relay_config_msg;
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            append_frame(buf, MergingMsgId::RelayConfigV1, msg);
        });
        if let Some(msg) = &self.slot.slot_start {
            self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                append_frame(buf, MergingMsgId::SlotStartV1, msg);
            });
            for ix in self.slot.mergeable_ixs.clone() {
                self.forward_decoded(ix, Some(token));
            }
        }
    }

    fn poll_sockets(&mut self) {
        // Split borrows: the connector is exclusively borrowed for the whole
        // poll, all reactions are buffered.
        let Self {
            connector,
            conns,
            slot,
            to_disconnect,
            to_register,
            handshaken,
            pongs,
            merged_ixs,
            merged_blocks,
            ..
        } = self;

        connector.poll_with(|event| match event {
            PollEvent::Accept { .. } => error!("unexpected inbound connection on merging tile"),
            PollEvent::Reconnect { token } => {
                info!(?token, "reconnected to merging builder");
                if let Some(conn) = conns.get_mut(&token) {
                    conn.reset();
                    to_register.push(token);
                }
            }
            PollEvent::Disconnect { token } => {
                warn!(?token, "merging builder disconnected");
                if let Some(conn) = conns.get_mut(&token) {
                    conn.reset();
                }
            }
            PollEvent::Message { token, payload, send_ts: _ } => {
                let header = match MergingFrameHeader::decode(payload) {
                    Ok(header) => header,
                    // extension ids must be ignored
                    Err(MergingHeaderError::ExtensionMsgId(_)) => return,
                    Err(err) => {
                        warn!(?token, %err, "bad merging frame, disconnecting");
                        to_disconnect.push(token);
                        return;
                    }
                };
                if header.is_zstd_compressed() {
                    warn!(?token, "zstd frame but compression was not negotiated, disconnecting");
                    to_disconnect.push(token);
                    return;
                }
                let body = &payload[MERGING_HEADER_SIZE..];
                match header.msg_id {
                    MergingMsgId::MergerAckV1 => {
                        let Ok(ack) = MergerAckV1::from_ssz_bytes(body) else {
                            warn!(?token, "undecodable ack, disconnecting");
                            to_disconnect.push(token);
                            return;
                        };
                        if ack.status.is_err() {
                            error!(
                                ?token,
                                msg = %String::from_utf8_lossy(&ack.error_msg),
                                "merging registration rejected"
                            );
                            to_disconnect.push(token);
                            return;
                        }
                        info!(?token, version = ack.version, "merging builder handshake ok");
                        if let Some(conn) = conns.get_mut(&token) {
                            conn.active = true;
                            conn.max_orders_per_slot = ack.max_orders_per_slot;
                            conn.max_frame_bytes = ack.max_frame_bytes;
                            handshaken.push(token);
                        }
                    }
                    MergingMsgId::PongV1 => {
                        if let Ok(pong) = PongV1::from_ssz_bytes(body) {
                            trace!(?token, nonce = pong.nonce, "merging pong");
                        }
                    }
                    MergingMsgId::PingV1 => {
                        if let Ok(ping) = PingV1::from_ssz_bytes(body) {
                            pongs.push((token, ping.nonce));
                        }
                    }
                    MergingMsgId::MergedBlockV1 => {
                        let Ok(merged) = MergedBlockV1::from_ssz_bytes(body) else {
                            warn!(?token, "undecodable merged block");
                            return;
                        };
                        if merged.slot != slot.bid_slot ||
                            !slot.appendable.contains(&merged.base_block_hash)
                        {
                            debug!(
                                ?token,
                                slot = merged.slot,
                                bid_slot = slot.bid_slot,
                                "stale or unknown merged block"
                            );
                            return;
                        }
                        // Builders only guarantee per-connection monotonicity;
                        // filter across builders so the stored merged bid
                        // never regresses.
                        let best =
                            slot.best_merged.entry(merged.base_block_hash).or_insert(U256::ZERO);
                        if merged.proposer_value <= *best {
                            return;
                        }
                        *best = merged.proposer_value;
                        let ix = merged_blocks.push(merged_block_to_response(merged));
                        merged_ixs.push(ix);
                    }
                    MergingMsgId::RejectV1 => {
                        if let Ok(reject) = RejectV1::from_ssz_bytes(body) {
                            warn!(
                                ?token,
                                slot = reject.slot,
                                code = ?reject.code,
                                subject = ?reject.subject,
                                msg = %String::from_utf8_lossy(&reject.msg),
                                "merging reject"
                            );
                        }
                    }
                    MergingMsgId::FatalV1 => {
                        if let Ok(fatal) = FatalV1::from_ssz_bytes(body) {
                            error!(
                                ?token,
                                code = ?fatal.code,
                                msg = %String::from_utf8_lossy(&fatal.msg),
                                "merging fatal, disconnecting"
                            );
                        }
                        to_disconnect.push(token);
                    }
                    other => {
                        warn!(?token, ?other, "unexpected merging msg, disconnecting");
                        to_disconnect.push(token);
                    }
                }
            }
        });

        for token in std::mem::take(&mut self.to_disconnect) {
            // outbound: schedules an auto-reconnect, which re-handshakes
            self.connector.disconnect(token);
            if let Some(conn) = self.conns.get_mut(&token) {
                conn.reset();
            }
        }
        for token in std::mem::take(&mut self.to_register) {
            self.send_registration(token);
        }
        for token in std::mem::take(&mut self.handshaken) {
            self.complete_handshake(token);
        }
        for (token, nonce) in std::mem::take(&mut self.pongs) {
            self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                append_frame(buf, MergingMsgId::PongV1, &PongV1 { nonce });
            });
        }
    }

    fn on_slot_msg(&mut self, msg: SlotMsg) {
        let Some(ev) = self.slot_events.get(msg.ix) else { return };
        let bid_slot = ev.bid_slot.as_u64();
        if bid_slot < self.slot.bid_slot {
            return;
        }
        if bid_slot > self.slot.bid_slot {
            self.slot = SlotState { bid_slot, ..Default::default() };
            for conn in self.conns.values_mut() {
                conn.reset_slot();
            }
            // sole producer; consumed indices are stale after the transition
            self.merged_blocks.clear();
        }

        // housekeeper sends incremental updates for the same slot
        if let Some(reg) = &ev.registration_data {
            self.slot.fee_recipient = Some(reg.entry.registration.message.fee_recipient);
        }
        for attr in &ev.payload_attributes {
            self.slot
                .attrs
                .insert(attr.parent_hash, attr.parent_beacon_block_root.unwrap_or_default());
        }
        self.maybe_start_slot();
    }

    fn maybe_start_slot(&mut self) {
        if self.slot.slot_start.is_some() {
            return;
        }
        let Some(fee_recipient) = self.slot.fee_recipient else { return };
        // single parent: builders cross-check against their synced head and
        // reject with HeadMismatch on competing forks
        let Some((&parent_hash, &parent_beacon_block_root)) = self.slot.attrs.iter().next() else {
            return;
        };
        let msg = SlotStartV1 {
            slot: self.slot.bid_slot,
            parent_hash,
            proposer_fee_recipient: fee_recipient,
            parent_beacon_block_root,
        };
        debug!(slot = self.slot.bid_slot, "merging slot start");

        for (token, conn) in self.conns.iter() {
            if conn.active {
                self.connector.write_or_enqueue_with(SendBehavior::Single(*token), |buf| {
                    append_frame(buf, MergingMsgId::SlotStartV1, &msg);
                });
            }
        }
        self.slot.slot_start = Some(msg);
    }

    /// Forwards the decoded submission at `ix` as a `MergeableBlockV1` to all
    /// active connections, or to `only` on re-handshake replay.
    fn forward_decoded(&mut self, ix: usize, only: Option<Token>) {
        if self.slot.slot_start.is_none() {
            return;
        }
        let Some(data) = self.decoded.get(ix) else { return };
        let sub = &data.submission_data;
        let Some(merging) = &sub.merging_data else { return };
        // mergeable submissions are always full (see decoder)
        let Submission::Full(signed) = &sub.submission else { return };
        // also guards replay ixs against the auctioneer's decoded.clear()
        if signed.message.slot != self.slot.bid_slot {
            return;
        }

        let mut merge_orders = Vec::with_capacity(merging.merge_orders.len());
        for order in &merging.merge_orders {
            match order_to_ref(order) {
                Some(r) => merge_orders.push(r),
                None => warn!("dropping merge order with out of range tx index"),
            }
        }
        let order_count = merge_orders.len() as u32;
        let block_hash = signed.message.block_hash;

        let msg = MergeableBlockV1 {
            slot: signed.message.slot,
            builder_pubkey: signed.message.builder_pubkey,
            block_value: signed.message.value,
            builder_address: merging.orders.origin,
            proposer_fee_recipient: signed.message.proposer_fee_recipient,
            parent_beacon_block_root: self
                .slot
                .attrs
                .get(&signed.message.parent_hash)
                .copied()
                .unwrap_or_default(),
            allow_appending: merging.allow_appending,
            merge_orders,
            execution_payload: payload_to_v3(&signed.execution_payload),
        };

        self.encode_buf.clear();
        append_frame(&mut self.encode_buf, MergingMsgId::MergeableBlockV1, &msg);

        if only.is_none() {
            if msg.allow_appending {
                self.slot.appendable.insert(block_hash);
            }
            self.slot.mergeable_ixs.push(ix);
        }

        let frame = &self.encode_buf;
        for (token, conn) in self.conns.iter_mut() {
            if !conn.active || only.is_some_and(|t| t != *token) {
                continue;
            }
            if conn.orders_sent.saturating_add(order_count) > conn.max_orders_per_slot ||
                frame.len() > conn.max_frame_bytes as usize
            {
                debug!(?token, %block_hash, "skipping mergeable block over builder limits");
                continue;
            }
            conn.orders_sent += order_count;
            if msg.allow_appending {
                conn.forwarded.insert(block_hash);
            }
            self.connector.write_or_enqueue_with(SendBehavior::Single(*token), |buf| {
                buf.extend_from_slice(frame);
            });
        }
    }

    fn on_top_bid(&mut self, top_bid: TopBidUpdate) {
        if top_bid.slot != self.slot.bid_slot || !self.slot.appendable.contains(&top_bid.block_hash)
        {
            return;
        }
        let msg = ActivateBaseBlockV1 { slot: top_bid.slot, block_hash: top_bid.block_hash };
        for (token, conn) in self.conns.iter_mut() {
            if !conn.active ||
                !conn.forwarded.contains(&top_bid.block_hash) ||
                conn.activated == Some(top_bid.block_hash)
            {
                continue;
            }
            conn.activated = Some(top_bid.block_hash);
            self.connector.write_or_enqueue_with(SendBehavior::Single(*token), |buf| {
                append_frame(buf, MergingMsgId::ActivateBaseBlockV1, &msg);
            });
        }
    }

    fn send_pings(&mut self) {
        self.ping_nonce += 1;
        let msg = PingV1 { nonce: self.ping_nonce };
        for (token, conn) in self.conns.iter() {
            if conn.active {
                self.connector.write_or_enqueue_with(SendBehavior::Single(*token), |buf| {
                    append_frame(buf, MergingMsgId::PingV1, &msg);
                });
            }
        }
    }
}
