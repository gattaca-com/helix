use std::sync::Arc;

use alloy_primitives::{B256, Bytes, U256, keccak256};
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
use helix_common::{BlockMergingTcpConfig, api::builder_api::TopBidUpdate, chain_info::ChainInfo};
use helix_tcp_types::merging::{
    MERGING_HEADER_SIZE, MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingHeaderError,
    MergingMsgId,
    builder_to_relay::{FatalV1, MergedBlockV1, RejectV1},
    control::{
        BuilderCollateral, MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1,
    },
    relay_to_builder::{ActivateBaseBlockV1, MergeableBlockV1, SlotStartV1},
};
use helix_types::{HydrationCache, Submission, payload_to_v3};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::{
    HelixSpine, SubmissionDataWithSpan,
    block_merging::{append_frame, merged_block_to_response, order_ref_hash, order_to_ref},
    housekeeper::SlotUpdate,
    simulator::BlockMergeResponse,
    spine::messages::{DecodedSubmission, MergedBlockMsg, SlotMsg},
};

const REDIAL_INTERVAL_S: u64 = 2;
const PING_INTERVAL_S: u64 = 5;

struct Endpoint {
    addr: std::net::SocketAddr,
    api_key: [u8; 16],
}

#[derive(Default)]
struct Conn {
    /// Handshake completed (ack received with ok status).
    active: bool,
    max_orders_per_slot: u32,
    max_frame_bytes: u32,
    /// Distinct order hashes (see `order_ref_hash`) sent on this connection
    /// this slot, against the builder's advertised `max_orders_per_slot`.
    /// Keyed by identity rather than counted per-announcement: the same
    /// order gets re-declared across every resubmission/every builder that
    /// also saw it, and a raw per-announcement count would exhaust the
    /// budget on that repetition alone rather than genuinely distinct
    /// orders.
    orders_sent: FxHashSet<B256>,
    /// Appendable block hashes forwarded on this connection this slot.
    forwarded: FxHashSet<B256>,
    activated: Option<B256>,
    /// Tx hashes already sent whole on this connection this slot; a repeat
    /// tx is forwarded as a hash reference instead. Cleared with the rest of
    /// the per-slot state so it never outlives the builder's own per-slot
    /// resolution cache, and on reconnect since a fresh connection can't
    /// resolve references to bytes it was never sent.
    sent_txs: FxHashSet<B256>,
}

impl Conn {
    fn reset(&mut self) {
        self.active = false;
        self.reset_slot();
    }

    fn reset_slot(&mut self) {
        self.orders_sent.clear();
        self.forwarded.clear();
        self.activated = None;
        self.sent_txs.clear();
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
    /// Appendable block hashes forwarded this slot.
    appendable: FxHashSet<B256>,
    /// Decoded ixs forwarded this slot, replayed on re-handshake.
    mergeable_ixs: Vec<usize>,
    /// base_block_hash -> best proposer_value across all builders.
    best_merged: FxHashMap<B256, U256>,
}

/// Per-slot counters, logged and reset on slot transition.
#[derive(Default)]
struct SlotStats {
    /// Mergeable frames built from full submissions.
    forwarded_full: usize,
    /// Mergeable frames built from dehydrated submissions after hydration.
    forwarded_hydrated: usize,
    hydration_failed: usize,
    skipped_no_slot_start: usize,
    skipped_wrong_slot: usize,
    skipped_no_merging_data: usize,
    /// Sends skipped over the builder's advertised limits.
    skipped_over_limits: usize,
    /// Merge orders dropped for an out of range tx index.
    orders_dropped: usize,
    /// Mergeable frames replayed on re-handshake.
    replayed: usize,
    merged_blocks: usize,
    merged_stale: usize,
    /// Merged blocks discarded because a better one was already stored.
    merged_regressed: usize,
    /// TopBidUpdate messages received for the current bid slot.
    top_bid_updates: usize,
    /// ActivateBaseBlockV1 frames sent.
    activations_sent: usize,
    /// Gaps between consecutive top bid updates, from bid sorter send
    /// timestamps.
    top_bid_gaps_ns: Vec<u64>,
    last_top_bid_ns: u64,
    /// Txs sent as full bytes: new to this connection this slot.
    tx_bytes_sent: usize,
    /// Txs sent as a hash reference: already sent whole earlier this slot.
    tx_refs_sent: usize,
}

pub struct BlockMergingTile {
    connector: TcpConnector,
    relay_id: Vec<u8>,
    relay_config_msg: RelayConfigV1,

    endpoint: Endpoint,
    token: Option<Token>,
    conn: Conn,
    slot: SlotState,
    stats: SlotStats,
    chain_info: ChainInfo,
    /// Rebuilds full payloads from dehydrated submissions; fed by every
    /// dehydrated submission this slot, cleared on slot transition.
    hydration_cache: HydrationCache,

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
            self.dial_endpoint();
        }
        if self.ping.fired() {
            self.send_pings();
        }

        adapter.consume(|msg: SlotMsg, _| self.on_slot_msg(msg));
        adapter.consume(|msg: DecodedSubmission, _| self.forward_decoded(msg.ix, None));
        adapter.consume(|top_bid: TopBidUpdate, _| self.on_top_bid(top_bid));
    }

    fn try_init(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) -> bool {
        self.dial_endpoint();
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
        chain_info: ChainInfo,
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

        let endpoint = Endpoint {
            addr: config.builder.addr,
            api_key: Uuid::parse_str(&config.builder.api_key)
                .expect("invalid block merging api key")
                .into_bytes(),
        };

        // TODO: enable telemetry once the per-connection shm queue leak is fixed
        // Disabled: per-connection shm queue leak, see tcp_bid_recv/mod.rs.
        let connector = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Disabled)
            .with_socket_buf_size(64 * 1024 * 1024)
            // Otherwise a stale message queued for the dead socket (e.g. an
            // activation or ping) gets replayed on the new one ahead of the
            // fresh MergerRegistrationV1, and the builder rejects it with
            // "expected registration" — killing the connection again.
            .with_drop_outbound_backlog_on_disconnect(true);

        Self {
            connector,
            relay_id: relay_id.into_bytes(),
            relay_config_msg,
            endpoint,
            token: None,
            conn: Conn::default(),
            slot: SlotState::default(),
            stats: SlotStats::default(),
            chain_info,
            hydration_cache: HydrationCache::new(),
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

    /// Dials the builder if not already connected. A failed initial `connect`
    /// is not retried by the connector (unlike an established conn, which
    /// auto-reconnects), so this runs on a repeater.
    fn dial_endpoint(&mut self) {
        if self.token.is_some() {
            return;
        }
        let addr = self.endpoint.addr;
        let Some(token) = self.connector.connect(addr) else {
            warn!(%addr, "failed to dial merging builder");
            return;
        };
        info!(%addr, ?token, "dialing merging builder");
        self.token = Some(token);
        self.conn = Conn::default();
        self.send_registration(token);
    }

    fn send_registration(&mut self, token: Token) {
        let msg = MergerRegistrationV1 {
            api_key: self.endpoint.api_key,
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
            token: my_token,
            conn,
            slot,
            stats,
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
                if *my_token == Some(token) {
                    conn.reset();
                    to_register.push(token);
                }
            }
            PollEvent::Disconnect { token } => {
                warn!(?token, "merging builder disconnected");
                if *my_token == Some(token) {
                    conn.reset();
                }
            }
            PollEvent::Message { token, payload, send_ts: _ } => {
                if *my_token != Some(token) {
                    return;
                }
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
                        conn.active = true;
                        conn.max_orders_per_slot = ack.max_orders_per_slot;
                        conn.max_frame_bytes = ack.max_frame_bytes;
                        handshaken.push(token);
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
                            stats.merged_stale += 1;
                            debug!(
                                ?token,
                                slot = merged.slot,
                                bid_slot = slot.bid_slot,
                                "stale or unknown merged block"
                            );
                            return;
                        }
                        // Builders only guarantee monotonicity within a
                        // connection; filter so the stored merged bid never
                        // regresses.
                        let best =
                            slot.best_merged.entry(merged.base_block_hash).or_insert(U256::ZERO);
                        if merged.proposer_value <= *best {
                            stats.merged_regressed += 1;
                            return;
                        }
                        *best = merged.proposer_value;
                        stats.merged_blocks += 1;
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
            if self.token == Some(token) {
                self.conn.reset();
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
            self.report_slot_stats();
            self.slot = SlotState { bid_slot, ..Default::default() };
            self.conn.reset_slot();
            self.hydration_cache.clear();
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

        if self.conn.active &&
            let Some(token) = self.token
        {
            self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                append_frame(buf, MergingMsgId::SlotStartV1, &msg);
            });
        }
        self.slot.slot_start = Some(msg);
    }

    fn report_slot_stats(&mut self) {
        let mut stats = std::mem::take(&mut self.stats);
        if self.slot.bid_slot == 0 {
            return;
        }
        info!(
            bid_slot = self.slot.bid_slot,
            top_bid_updates = stats.top_bid_updates,
            top_bid_median_gap_ms = Self::median(&mut stats.top_bid_gaps_ns) as f64 / 1e6,
            activations_sent = stats.activations_sent,
            forwarded_full = stats.forwarded_full,
            forwarded_hydrated = stats.forwarded_hydrated,
            hydration_failed = stats.hydration_failed,
            skipped_no_slot_start = stats.skipped_no_slot_start,
            skipped_wrong_slot = stats.skipped_wrong_slot,
            skipped_no_merging_data = stats.skipped_no_merging_data,
            skipped_over_limits = stats.skipped_over_limits,
            orders_dropped = stats.orders_dropped,
            replayed = stats.replayed,
            merged_blocks = stats.merged_blocks,
            merged_stale = stats.merged_stale,
            merged_regressed = stats.merged_regressed,
            appendable_blocks = self.slot.appendable.len(),
            hydration_txs = self.hydration_cache.tx_count(),
            hydration_builders = self.hydration_cache.builder_count(),
            tx_bytes_sent = stats.tx_bytes_sent,
            tx_refs_sent = stats.tx_refs_sent,
            "block merging slot stats"
        );
    }

    /// Even when a submission is not forwarded, its full transactions and
    /// blobs must enter the cache so later dehydrated submissions from the
    /// same builder can resolve their references.
    fn feed_cache(&mut self, submission: &Submission) {
        if let Submission::Dehydrated(d) = submission {
            self.hydration_cache.feed(d);
        }
    }

    /// Forwards the decoded submission at `ix` as a `MergeableBlockV1`, or
    /// replays it to `only` on re-handshake.
    fn forward_decoded(&mut self, ix: usize, only: Option<Token>) {
        let is_replay = only.is_some();
        if is_replay {
            self.stats.replayed += 1;
        }
        let Some(data) = self.decoded.get(ix) else { return };
        let sub = &data.submission_data;

        if self.slot.slot_start.is_none() {
            self.stats.skipped_no_slot_start += 1;
            self.feed_cache(&sub.submission);
            return;
        }
        // also guards replay ixs against the auctioneer's decoded.clear()
        if sub.submission.bid_slot() != self.slot.bid_slot {
            self.stats.skipped_wrong_slot += 1;
            if !is_replay {
                self.feed_cache(&sub.submission);
            }
            return;
        }
        let Some(merging) = &sub.merging_data else {
            self.stats.skipped_no_merging_data += 1;
            self.feed_cache(&sub.submission);
            return;
        };

        let hydrated;
        let signed = match &sub.submission {
            Submission::Full(signed) => {
                if !is_replay {
                    self.stats.forwarded_full += 1;
                }
                signed
            }
            // hydrating also feeds this submission's new txs into the cache
            Submission::Dehydrated(d) => {
                match self.hydration_cache.hydrate(d.clone(), self.chain_info.max_blobs_per_block())
                {
                    Ok(h) => {
                        if !is_replay {
                            self.stats.forwarded_hydrated += 1;
                        }
                        hydrated = h.submission;
                        &hydrated
                    }
                    Err(_) => {
                        self.stats.hydration_failed += 1;
                        return;
                    }
                }
            }
        };

        let mut merge_orders = Vec::with_capacity(merging.merge_orders.len());
        for order in &merging.merge_orders {
            match order_to_ref(order) {
                Some(r) => merge_orders.push(r),
                None => self.stats.orders_dropped += 1,
            }
        }
        let block_hash = signed.message.block_hash;

        let mut msg = MergeableBlockV1 {
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

        // Hashed once up front: reused both to decide full-bytes-vs-reference
        // below and to key each order's distinct identity (`order_ref_hash`),
        // so a tx forwarded as a hash reference still contributes the same
        // identity it would have as a full tx.
        let tx_hashes: Vec<B256> = msg
            .execution_payload
            .payload_inner
            .payload_inner
            .transactions
            .iter()
            .map(|tx| keccak256(tx.as_ref()))
            .collect();

        // Dehydrate relative to this connection: a tx already sent whole
        // earlier this slot (from this block or any other, any builder) goes
        // out as a hash reference instead. See MergeableBlockV1's doc comment
        // for the wire convention.
        for (tx, &hash) in msg
            .execution_payload
            .payload_inner
            .payload_inner
            .transactions
            .iter_mut()
            .zip(&tx_hashes)
        {
            if self.conn.sent_txs.insert(hash) {
                self.stats.tx_bytes_sent += 1;
            } else {
                *tx = Bytes::copy_from_slice(hash.as_slice());
                self.stats.tx_refs_sent += 1;
            }
        }

        // A repeat announcement of the same order — the common case, since
        // builders resubmit near-identical blocks as their bid ratchets, and
        // popular public txs show up in most builders' blocks — must not
        // count again against the per-slot budget; only genuinely new
        // distinct orders should.
        let order_hashes: Vec<B256> = msg
            .merge_orders
            .iter()
            .map(|order_ref| order_ref_hash(order_ref, &tx_hashes))
            .collect();

        self.encode_buf.clear();
        append_frame(&mut self.encode_buf, MergingMsgId::MergeableBlockV1, &msg);

        if only.is_none() {
            if msg.allow_appending {
                self.slot.appendable.insert(block_hash);
            }
            self.slot.mergeable_ixs.push(ix);
        }

        let Some(token) = self.token else { return };
        if !self.conn.active || only.is_some_and(|t| t != token) {
            return;
        }
        let frame = &self.encode_buf;
        let new_orders =
            order_hashes.iter().filter(|hash| !self.conn.orders_sent.contains(*hash)).count()
                as u32;
        if (self.conn.orders_sent.len() as u32).saturating_add(new_orders) >
            self.conn.max_orders_per_slot ||
            frame.len() > self.conn.max_frame_bytes as usize
        {
            self.stats.skipped_over_limits += 1;
            debug!(?token, %block_hash, "skipping mergeable block over builder limits");
            return;
        }
        self.conn.orders_sent.extend(order_hashes);
        if msg.allow_appending {
            self.conn.forwarded.insert(block_hash);
        }
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            buf.extend_from_slice(frame);
        });
    }

    fn on_top_bid(&mut self, top_bid: TopBidUpdate) {
        if top_bid.slot != self.slot.bid_slot {
            return;
        }
        self.stats.top_bid_updates += 1;
        if self.stats.last_top_bid_ns > 0 {
            self.stats
                .top_bid_gaps_ns
                .push(top_bid.timestamp.saturating_sub(self.stats.last_top_bid_ns));
        }
        self.stats.last_top_bid_ns = top_bid.timestamp;

        if !self.slot.appendable.contains(&top_bid.block_hash) {
            return;
        }
        let Some(token) = self.token else { return };
        if !self.conn.active ||
            !self.conn.forwarded.contains(&top_bid.block_hash) ||
            self.conn.activated == Some(top_bid.block_hash)
        {
            return;
        }
        self.conn.activated = Some(top_bid.block_hash);
        self.stats.activations_sent += 1;
        let msg = ActivateBaseBlockV1 { slot: top_bid.slot, block_hash: top_bid.block_hash };
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            append_frame(buf, MergingMsgId::ActivateBaseBlockV1, &msg);
        });
    }

    /// Median of unsorted samples; 0 if empty.
    fn median(samples: &mut [u64]) -> u64 {
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        let mid = samples.len() / 2;
        if samples.len().is_multiple_of(2) {
            (samples[mid - 1] + samples[mid]) / 2
        } else {
            samples[mid]
        }
    }

    fn send_pings(&mut self) {
        self.ping_nonce += 1;
        let Some(token) = self.token else { return };
        if !self.conn.active {
            return;
        }
        let msg = PingV1 { nonce: self.ping_nonce };
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            append_frame(buf, MergingMsgId::PingV1, &msg);
        });
    }
}
