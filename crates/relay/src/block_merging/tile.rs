use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::{B256, Bytes, keccak256};
use flux::{
    spine::SpineProducers,
    tile::Tile,
    timing::{Duration, Nanos, Repeater},
};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use flux_utils::SharedVector;
use helix_common::{
    BlockMergingTcpConfig,
    api::builder_api::{InclusionListWithMetadata, TopBidUpdate},
    chain_info::ChainInfo,
    simulator::BlockSimError,
    utils::alert_discord,
};
use helix_tcp_types::merging::{
    MERGING_HEADER_SIZE, MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingHeaderError,
    MergingMsgId,
    builder_to_relay::{FatalV1, MergedBlockV1, RejectCode, RejectSubject, RejectV1},
    control::{
        BuilderCollateral, MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1,
    },
    order::MergeOrderRef,
    relay_to_builder::{ActivateBaseBlockV1, MergeableBlockV1, SlotStartV1},
};
use helix_types::{BlobWithMetadata, HydrationCache, Submission, payload_to_v3};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::{
    HelixSpine, SimRequest, SimResult, SubmissionDataWithSpan,
    block_merging::{
        append_frame, appended_tx_hashes, merged_block_to_response, order_ref_hash, order_to_ref,
        submission_blob_sidecars,
        unbundling::{OrderTxs, find_unbundled_txs},
    },
    housekeeper::SlotUpdate,
    simulator::{BlockMergeResponse, MergedValidationRequest, tile::MergedSimulationResultInner},
    spine::messages::{
        DecodedSubmission, FromSimMsg, MergedBlockMsg, SlotMsg, ToSimKind, ToSimMsg,
    },
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
    /// Builder's head is past our bid slot, so it refuses everything until we catch up.
    builder_ahead: bool,
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
        self.builder_ahead = false;
    }
}

#[derive(Default)]
struct SlotState {
    bid_slot: u64,
    /// Set once registration + payload attributes arrived and the broadcast
    /// went out; cached for handshake replay.
    slot_start: Option<SlotStartV1>,
    fee_recipient: Option<alloy_primitives::Address>,
    /// Registered gas limit of the current proposer, for merged-block simulation requests.
    registered_gas_limit: Option<u64>,
    /// Current proposer's blacklist-filtering preference, for merged-block simulation requests.
    apply_blacklist: Option<bool>,
    /// Current inclusion list, for merged-block simulation requests.
    inclusion_list: Option<InclusionListWithMetadata>,
    /// parent_hash -> parent_beacon_block_root.
    attrs: FxHashMap<B256, B256>,
    /// Appendable block hashes forwarded this slot.
    appendable: FxHashSet<B256>,
    /// Events replayed on re-handshake.
    replay_log: Vec<ReplayEvent>,
    /// Orders actually sent to the merge builder this slot, for the
    /// unbundling check on incoming merged blocks.
    order_txs: Vec<OrderTxs>,
}

#[derive(Clone, Copy)]
enum ReplayEvent {
    Forward(usize),
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
    /// Sends skipped because the builder is already past this slot.
    skipped_builder_ahead: usize,
    /// Sends skipped over the builder's advertised limits.
    skipped_over_limits: usize,
    /// Merge orders dropped for an out of range tx index.
    orders_dropped: usize,
    orders_forwarded: usize,
    orders_forwarded_latest_only: usize,
    /// Mergeable frames replayed on re-handshake.
    replayed: usize,
    merged_blocks: usize,
    merged_stale: usize,
    /// Merged blocks dropped because an appended blob's sidecar wasn't in our cache.
    merged_blob_missing: usize,
    /// Merged blocks whose simulation was skipped because a required piece of this
    /// slot's state (beacon parent root, fee recipient, or registered gas limit)
    /// isn't known yet.
    merged_slot_data_missing: usize,
    /// Merged blocks dropped because the builder broke an order's atomicity.
    merged_unbundled: usize,
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
    /// Blob sidecars from every submission with mergeable orders this slot, keyed by KZG
    /// versioned hash, so an appended blob tx can be re-attached regardless of which
    /// builder's submission it originated from. Cleared on slot transition.
    blob_sidecars: FxHashMap<B256, BlobWithMetadata>,
    /// Memoizes keccak256(tx bytes), shared by the outgoing per-submission
    /// hashing and the incoming merged-block check. Cleared on slot transition.
    tx_hash_cache: FxHashMap<Bytes, B256>,

    redial: Repeater,
    ping: Repeater,
    ping_nonce: u64,

    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    slot_events: Arc<SharedVector<SlotUpdate>>,
    merged_blocks: Arc<SharedVector<BlockMergeResponse>>,
    sim_requests: Arc<SharedVector<SimRequest>>,
    sim_results: Arc<SharedVector<SimResult>>,
    /// Admin-toggled kill switch. The connection itself (dial, handshake,
    /// ping/pong) is unaffected — only an admin can set this back to `true`
    /// (never automatic). While `false`, the tile stops forwarding
    /// mergeable blocks and top-bid activations, and silently drops any
    /// merged blocks it receives.
    block_merging_enabled: Arc<AtomicBool>,

    // Buffered during `poll_with` (the connector is exclusively borrowed
    // there), drained right after.
    to_disconnect: Vec<Token>,
    to_register: Vec<Token>,
    handshaken: Vec<Token>,
    pongs: Vec<(Token, u64)>,
    rejects: Vec<(Token, RejectV1)>,
    merged_ixs: Vec<usize>,
    merge_sim_ixs: Vec<usize>,
    encode_buf: Vec<u8>,
    // Scratch space for `find_unbundled_txs`, reused across calls.
    unbundled_scratch_bundled: Vec<bool>,
    unbundled_scratch_covered: Vec<bool>,
}

impl Tile<HelixSpine> for BlockMergingTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.poll_sockets();

        for ix in std::mem::take(&mut self.merged_ixs) {
            adapter.producers.produce(MergedBlockMsg { ix });
        }
        for ix in std::mem::take(&mut self.merge_sim_ixs) {
            adapter.producers.produce(ToSimMsg { kind: ToSimKind::Request, ix, bid_slot: 0 });
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
        adapter.consume(|msg: FromSimMsg, _| self.on_merge_sim_result(msg));
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

/// Validates and converts an incoming `MergedBlockV1` into a response to
/// forward to the auctioneer, or `None` if it's dropped (disabled, stale,
/// regressed, missing a blob sidecar, or unbundled by the builder).
/// Extracted from `poll_sockets` so this compute is unit-testable without a
/// real connection: `enabled` is checked first, before any state mutation,
/// so a disabled tile silently drops every merged block it receives.
#[allow(clippy::too_many_arguments)]
fn handle_merged_block(
    enabled: bool,
    token: Token,
    merged: MergedBlockV1,
    slot: &mut SlotState,
    stats: &mut SlotStats,
    blob_sidecars: &FxHashMap<B256, BlobWithMetadata>,
    tx_hash_cache: &mut FxHashMap<Bytes, B256>,
    unbundled_scratch_bundled: &mut Vec<bool>,
    unbundled_scratch_covered: &mut Vec<bool>,
    max_blobs_per_block: usize,
) -> Option<BlockMergeResponse> {
    if !enabled {
        return None;
    }
    if merged.slot != slot.bid_slot || !slot.appendable.contains(&merged.base_block_hash) {
        stats.merged_stale += 1;
        debug!(
            ?token,
            slot = merged.slot,
            bid_slot = slot.bid_slot,
            "stale or unknown merged block"
        );
        return None;
    }
    let Some(response) = merged_block_to_response(merged, blob_sidecars, max_blobs_per_block)
    else {
        stats.merged_blob_missing += 1;
        warn!(
            ?token,
            "could not build merge response (missing blob sidecar or invalid payload), dropping \
             merged block"
        );
        return None;
    };
    let appended = appended_tx_hashes(&response.builder_inclusions);
    let appended_txs: Vec<B256> = response
        .execution_payload
        .transactions
        .iter()
        .map(|tx| *tx_hash_cache.entry(tx.0.clone()).or_insert_with(|| keccak256(tx.as_ref())))
        .filter(|hash| appended.contains(hash))
        .collect();
    let unbundled = find_unbundled_txs(
        &appended_txs,
        &slot.order_txs,
        unbundled_scratch_bundled,
        unbundled_scratch_covered,
    );
    if !unbundled.is_empty() {
        stats.merged_unbundled += 1;
        warn!(
            ?token,
            count = unbundled.len(),
            "merge builder unbundled an order, dropping merged block"
        );
        return None;
    }
    stats.merged_blocks += 1;
    Some(response)
}

/// Builds the simulation request for a freshly accepted merged block, resolving
/// `parent_beacon_block_root`, `proposer_fee_recipient`, and `registered_gas_limit`
/// from this slot's cached state. `None` if any of them isn't known yet, rather than
/// silently defaulting: the external validator checks the merge builder's payment tx
/// against `proposer_fee_recipient` and re-executes with `parent_beacon_block_root`
/// (written into the EIP-4788 beacon-roots contract), so a zero-defaulted value
/// produces a genuine validation failure that isn't actually the merge builder's
/// fault.
fn merged_validation_request(
    base_block_hash: B256,
    parent_hash: B256,
    slot: &SlotState,
    merged_block_ix: usize,
    receive_ns: u64,
) -> Option<MergedValidationRequest> {
    let parent_beacon_block_root = *slot.attrs.get(&parent_hash)?;
    let proposer_fee_recipient = slot.fee_recipient?;
    let registered_gas_limit = slot.registered_gas_limit?;
    Some(MergedValidationRequest {
        merged_block_ix,
        base_block_hash,
        slot: slot.bid_slot,
        parent_beacon_block_root,
        proposer_fee_recipient,
        registered_gas_limit,
        apply_blacklist: slot.apply_blacklist.unwrap_or(true),
        inclusion_list: slot.inclusion_list.clone().unwrap_or_default(),
        receive_ns,
    })
}

/// Whether a merged-block simulation failure is attributable to the merge builder, as
/// opposed to a relay/simulator-side infra hiccup. Builds on `is_demotable()` (the same
/// logic that decides whether a failed bid-submission simulation demotes its builder) but
/// additionally excludes internal channel/queue failures, which are never the builder's
/// fault even though `is_demotable()` -- calibrated for bid-submission demotion -- doesn't
/// exclude them.
fn is_merge_builder_attributable(err: &BlockSimError) -> bool {
    err.is_demotable() &&
        !matches!(
            err,
            BlockSimError::SendError |
                BlockSimError::SimulationDropped |
                BlockSimError::HydrationMiss
        )
}

/// Decides whether a merged-block simulation result should disable block merging.
/// Returns the block's hash and the failure reason to report if so.
fn merge_sim_disable_check(
    result: &MergedSimulationResultInner,
    merged_blocks: &SharedVector<BlockMergeResponse>,
) -> Option<(B256, BlockSimError)> {
    let Err(err) = &result.result else { return None };
    if !is_merge_builder_attributable(err) {
        return None;
    }
    let block_hash = merged_blocks
        .get(result.merged_block_ix)
        .map(|r| r.execution_payload.block_hash)
        .unwrap_or_default();
    Some((block_hash, err.clone()))
}

impl BlockMergingTile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: BlockMergingTcpConfig,
        relay_id: String,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        slot_events: Arc<SharedVector<SlotUpdate>>,
        merged_blocks: Arc<SharedVector<BlockMergeResponse>>,
        sim_requests: Arc<SharedVector<SimRequest>>,
        sim_results: Arc<SharedVector<SimResult>>,
        chain_info: ChainInfo,
        block_merging_enabled: Arc<AtomicBool>,
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
            blob_sidecars: FxHashMap::default(),
            tx_hash_cache: FxHashMap::default(),
            redial: Repeater::every(Duration::from_secs(REDIAL_INTERVAL_S)),
            ping: Repeater::every(Duration::from_secs(PING_INTERVAL_S)),
            ping_nonce: 0,
            decoded,
            slot_events,
            merged_blocks,
            sim_requests,
            sim_results,
            block_merging_enabled,
            to_disconnect: Vec::new(),
            to_register: Vec::new(),
            handshaken: Vec::new(),
            pongs: Vec::new(),
            rejects: Vec::new(),
            merged_ixs: Vec::new(),
            merge_sim_ixs: Vec::new(),
            encode_buf: Vec::new(),
            unbundled_scratch_bundled: Vec::new(),
            unbundled_scratch_covered: Vec::new(),
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
            for event in self.slot.replay_log.clone() {
                match event {
                    ReplayEvent::Forward(ix) => self.forward_decoded(ix, Some(token)),
                }
            }
        }
    }

    fn poll_sockets(&mut self) {
        let enabled = self.block_merging_enabled.load(Ordering::Relaxed);
        let max_blobs_per_block = self.chain_info.max_blobs_per_block();

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
            rejects,
            merged_ixs,
            merged_blocks,
            sim_requests,
            merge_sim_ixs,
            blob_sidecars,
            tx_hash_cache,
            unbundled_scratch_bundled,
            unbundled_scratch_covered,
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
                        if let Some(response) = handle_merged_block(
                            enabled,
                            token,
                            merged,
                            slot,
                            stats,
                            blob_sidecars,
                            tx_hash_cache,
                            unbundled_scratch_bundled,
                            unbundled_scratch_covered,
                            max_blobs_per_block,
                        ) {
                            let base_block_hash = response.base_block_hash;
                            let parent_hash = response.execution_payload.parent_hash;
                            let ix = merged_blocks.push(response);
                            merged_ixs.push(ix);

                            match merged_validation_request(
                                base_block_hash,
                                parent_hash,
                                slot,
                                ix,
                                Nanos::now().0,
                            ) {
                                Some(sim_req) => {
                                    let sim_ix = sim_requests
                                        .push(SimRequest::ValidateMerged(Box::new(sim_req)));
                                    merge_sim_ixs.push(sim_ix);
                                }
                                None => {
                                    stats.merged_slot_data_missing += 1;
                                    warn!(
                                        ?token,
                                        %parent_hash,
                                        "beacon parent root, fee recipient, or gas limit not \
                                         yet known for this slot, skipping merged block \
                                         simulation"
                                    );
                                }
                            }
                        }
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
                            rejects.push((token, reject));
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
        for (token, reject) in std::mem::take(&mut self.rejects) {
            self.on_reject(token, reject);
        }
    }

    /// `StaleSlot` and a later `reject.slot` both mean the builder is ahead of us.
    fn on_reject(&mut self, token: Token, reject: RejectV1) {
        if self.token != Some(token) {
            return;
        }
        if matches!(reject.code, RejectCode::StaleSlot) || reject.slot > self.slot.bid_slot {
            self.conn.builder_ahead = true;
        }
        // A named block is one the builder does not hold, so it must not be activated.
        if let RejectSubject::BlockHash(hash) = reject.subject {
            self.conn.forwarded.remove(&hash);
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
            self.blob_sidecars.clear();
            self.tx_hash_cache.clear();
            // sole producer; consumed indices are stale after the transition
            self.merged_blocks.clear();
        }

        // housekeeper sends incremental updates for the same slot
        if let Some(reg) = &ev.registration_data {
            self.slot.fee_recipient = Some(reg.entry.registration.message.fee_recipient);
            self.slot.registered_gas_limit = Some(reg.entry.registration.message.gas_limit);
            self.slot.apply_blacklist = Some(reg.entry.preferences.filtering.is_regional());
        }
        if let Some(il) = &ev.il {
            self.slot.inclusion_list = Some(il.clone());
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
            skipped_builder_ahead = stats.skipped_builder_ahead,
            skipped_over_limits = stats.skipped_over_limits,
            orders_dropped = stats.orders_dropped,
            orders_forwarded = stats.orders_forwarded,
            orders_forwarded_latest_only = stats.orders_forwarded_latest_only,
            replayed = stats.replayed,
            merged_blocks = stats.merged_blocks,
            merged_stale = stats.merged_stale,
            merged_blob_missing = stats.merged_blob_missing,
            merged_slot_data_missing = stats.merged_slot_data_missing,
            merged_unbundled = stats.merged_unbundled,
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
    /// replays it to `only` on re-handshake. A no-op while block merging is
    /// administratively disabled.
    fn forward_decoded(&mut self, ix: usize, only: Option<Token>) {
        if !self.block_merging_enabled.load(Ordering::Relaxed) {
            return;
        }
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
        // gattaca-com/helix#538: pointless, the builder refuses this slot.
        if self.conn.builder_ahead {
            self.stats.skipped_builder_ahead += 1;
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

        // Cache this submission's own blob sidecars so any of its blob txs that get merged
        // into another builder's base block can be re-attached from here later.
        self.blob_sidecars.extend(submission_blob_sidecars(&signed.blobs_bundle()));

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
            builder_address: merging.builder_address,
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
            .map(|tx| {
                *self.tx_hash_cache.entry(tx.clone()).or_insert_with(|| keccak256(tx.as_ref()))
            })
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
            self.slot.replay_log.push(ReplayEvent::Forward(ix));
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
        if !is_replay {
            self.stats.orders_forwarded += msg.merge_orders.len();
            self.stats.orders_forwarded_latest_only += msg
                .merge_orders
                .iter()
                .filter(|o| matches!(o, MergeOrderRef::Bundle(b) if b.latest_only))
                .count();
        }
        self.conn.orders_sent.extend(order_hashes);
        self.slot
            .order_txs
            .extend(msg.merge_orders.iter().map(|o| OrderTxs::from_ref(o, &tx_hashes)));
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

        if !self.block_merging_enabled.load(Ordering::Relaxed) {
            return;
        }
        if !self.slot.appendable.contains(&top_bid.block_hash) {
            return;
        }
        // Activating past our slot is what produces the builder's `UnknownBaseBlock`.
        if self.conn.builder_ahead {
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

    /// Ignores results for anything other than this tile's own `ValidateMerged` requests
    /// (the `from_sim` queue also carries the auctioneer's ordinary submission-validation
    /// results). On a builder-attributable failure, disables block merging -- this alone
    /// triggers the existing force-disconnect gating in `poll_sockets`/`dial_endpoint`, so
    /// no separate disconnect call is needed here. Nothing re-enables the flag except the
    /// admin API.
    fn on_merge_sim_result(&mut self, msg: FromSimMsg) {
        let Some(result) = self.sim_results.get(msg.ix) else {
            error!(?msg, "sim outbound payload not found");
            return;
        };
        let SimResult::ValidateMerged((_, Some(inner))) = result.as_ref() else { return };
        let Some((block_hash, err)) = merge_sim_disable_check(inner, &self.merged_blocks) else {
            return;
        };

        self.block_merging_enabled.store(false, Ordering::Relaxed);
        error!(
            %block_hash,
            %err,
            endpoint = %self.endpoint.addr,
            "merged block simulation failed, disabling block merging"
        );
        alert_discord(&format!(
            "CRITICAL: block merging disabled -- merged block simulation failed for block \
             {block_hash:#x} from merge builder {} ({err})",
            self.endpoint.addr
        ));
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{Address, Bloom, U256};
    use alloy_rpc_types::{
        beacon::{BlsPublicKey, requests::ExecutionRequestsV4},
        engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
    };
    use flux::timing::Nanos;
    use helix_common::{
        MergingBuilderCollateral, MergingBuilderEndpoint, SubmissionTrace,
        decoder::{Encoding, SubmissionDecoderParams},
    };
    use helix_tcp_types::{MergeType, merging::builder_to_relay::MergeTraceV1};
    use helix_types::{
        BlobsBundle, BlockMergingData, BlsPublicKeyBytes, BuilderInclusionResult, Compression,
        ExecutionPayload, ExecutionRequests, ForkName, MergedBlockTrace, SignedBidSubmission,
        SubmissionVersion, TestRandom, TestRandomSeed, dehydrated_submission_with_txs_for_test,
        full_tx_for_test,
    };
    use rand::{SeedableRng, rngs::SmallRng};

    use super::*;
    use crate::{SubmissionRef, auctioneer::SubmissionData};

    fn merge_response(
        payload: ExecutionPayload,
        proposer_value: U256,
        blobs: Vec<BlobWithMetadata>,
    ) -> BlockMergeResponse {
        let mut blobs_bundle = BlobsBundle::default();
        for blob in blobs {
            blobs_bundle.push_blob(blob.commitment, &blob.proofs, blob.blob, 9).unwrap();
        }
        BlockMergeResponse {
            base_block_hash: payload.parent_hash,
            execution_payload: payload,
            execution_requests: ExecutionRequests::default(),
            blobs_bundle,
            proposer_value,
            base_builder_revenue: U256::ZERO,
            relay_revenue: U256::ZERO,
            builder_inclusions: Default::default(),
            base_payment_tx_index: 0,
            trace: MergedBlockTrace::default(),
        }
    }

    fn inclusion(txs: Vec<B256>) -> BuilderInclusionResult {
        BuilderInclusionResult { contribution: U256::ZERO, revenue: U256::ZERO, txs }
    }

    #[test]
    fn appended_tx_hashes_collects_across_builders() {
        let tx_a = B256::repeat_byte(1);
        let tx_b = B256::repeat_byte(2);
        let tx_c = B256::repeat_byte(3);
        let builder_inclusions = HashMap::from([
            (Address::repeat_byte(0xa), inclusion(vec![tx_a, tx_b])),
            (Address::repeat_byte(0xb), inclusion(vec![tx_c])),
        ]);

        let appended = appended_tx_hashes(&builder_inclusions);

        assert_eq!(appended, FxHashSet::from_iter([tx_a, tx_b, tx_c]));
    }

    #[test]
    fn appended_tx_hashes_empty_when_nothing_was_appended() {
        assert!(appended_tx_hashes(&HashMap::new()).is_empty());
    }

    // Regression test for a false-positive class: an order sharing a tx hash
    // with the base block's own (untouched) content, that was never itself
    // satisfied, must not flag that base-block tx as unbundled. Filtering to
    // `appended_tx_hashes` before the check removes base content from
    // consideration entirely, so an unrelated, never-applied order can no
    // longer explain (or fail to explain) it.
    #[test]
    fn filtering_to_appended_txs_ignores_base_block_content() {
        let base_tx = B256::repeat_byte(1);
        let never_appended_tx = B256::repeat_byte(2);
        let appended_tx = B256::repeat_byte(3);

        // An unrelated, never-applied bundle that happens to share `base_tx`
        // with the base block's own plain content.
        let foreign_unsatisfied_order = OrderTxs::new(vec![base_tx, never_appended_tx], []);
        // The order actually applied by the merge builder.
        let applied_order = OrderTxs::new(vec![appended_tx], []);

        let builder_inclusions =
            HashMap::from([(Address::repeat_byte(0xa), inclusion(vec![appended_tx]))]);
        let appended = appended_tx_hashes(&builder_inclusions);

        let full_final_txs = vec![base_tx, appended_tx];
        let filtered_final_txs: Vec<B256> =
            full_final_txs.iter().copied().filter(|h| appended.contains(h)).collect();

        let orders = [foreign_unsatisfied_order, applied_order];
        assert_eq!(
            find_unbundled_txs(&filtered_final_txs, &orders, &mut Vec::new(), &mut Vec::new()),
            Vec::<B256>::new(),
        );
    }

    fn test_tile(enabled: bool) -> BlockMergingTile {
        let config = BlockMergingTcpConfig {
            builder: MergingBuilderEndpoint {
                addr: "127.0.0.1:1".parse().unwrap(),
                api_key: Uuid::nil().to_string(),
            },
            relay_fee_recipient: Address::ZERO,
            multisend_contract: Address::ZERO,
            relay_bps: 0,
            merged_builder_bps: 0,
            winning_builder_bps: 0,
            distribution_gas_limit: 140_000,
            builder_collaterals: vec![MergingBuilderCollateral {
                builder_coinbase: Address::ZERO,
                collateral_safe: Address::ZERO,
            }],
        };
        BlockMergingTile::new(
            config,
            "test-relay".to_string(),
            Arc::new(SharedVector::default()),
            Arc::new(SharedVector::default()),
            Arc::new(SharedVector::default()),
            Arc::new(SharedVector::default()),
            Arc::new(SharedVector::default()),
            ChainInfo::default(),
            Arc::new(AtomicBool::new(enabled)),
        )
    }

    /// A decoded submission carrying merging data for `bid_slot`/`block_hash`, with no merge
    /// orders — the per-slot order-budget/unbundling logic isn't under test here.
    fn test_submission(
        bid_slot: u64,
        block_hash: B256,
        allow_appending: bool,
    ) -> SubmissionDataWithSpan {
        let mut signed = SignedBidSubmission::test_random();
        signed.message.slot = bid_slot;
        signed.message.block_hash = block_hash;
        // `TestRandom` for `BlobsBundle` doesn't respect the
        // proofs/blobs/commitments length invariant (see the #[ignore]d
        // `fulu_bid_submission*` tests in helix-types) and panics on use;
        // this submission carries no blobs so it isn't touched.
        signed.blobs_bundle = Arc::new(Default::default());
        let submission_data = SubmissionData {
            submission_ref: SubmissionRef::Internal,
            submission: Submission::Full(signed),
            merging_data: Some(BlockMergingData {
                allow_appending,
                builder_address: Address::ZERO,
                merge_orders: vec![],
            }),
            bid_adjustment_data: None,
            version: SubmissionVersion::new(0, None),
            withdrawals_root: B256::ZERO,
            trace: SubmissionTrace::default(),
            decoder_params: SubmissionDecoderParams {
                compression: Compression::None,
                encoding: Encoding::Ssz,
                merge_type: MergeType::default(),
                is_dehydrated: false,
                with_mergeable_data: true,
                with_adjustments: false,
                mark_all_txs_mergeable: false,
                fork_name: ForkName::Deneb,
            },
            is_pessimistic: false,
        };
        SubmissionDataWithSpan { submission_data, span: tracing::Span::none(), sent_at: Nanos(0) }
    }

    #[test]
    fn forward_decoded_noop_when_disabled() {
        let mut tile = test_tile(false);
        tile.slot.bid_slot = 5;
        tile.slot.slot_start = Some(SlotStartV1 {
            slot: 5,
            parent_hash: B256::ZERO,
            proposer_fee_recipient: Address::ZERO,
            parent_beacon_block_root: B256::ZERO,
        });
        let block_hash = B256::repeat_byte(7);
        let ix = tile.decoded.push(test_submission(5, block_hash, true));

        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.is_empty());
        assert!(tile.slot.replay_log.is_empty());
    }

    #[test]
    fn forward_decoded_tracks_when_enabled() {
        let mut tile = test_tile(true);
        tile.slot.bid_slot = 5;
        tile.slot.slot_start = Some(SlotStartV1 {
            slot: 5,
            parent_hash: B256::ZERO,
            proposer_fee_recipient: Address::ZERO,
            parent_beacon_block_root: B256::ZERO,
        });
        let block_hash = B256::repeat_byte(7);
        let ix = tile.decoded.push(test_submission(5, block_hash, true));

        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.contains(&block_hash));
        assert_eq!(tile.slot.replay_log.len(), 1);
    }

    #[test]
    fn on_top_bid_skips_activation_when_disabled() {
        let mut tile = test_tile(false);
        let block_hash = B256::repeat_byte(3);
        tile.slot.bid_slot = 5;
        tile.slot.appendable.insert(block_hash);
        tile.conn.forwarded.insert(block_hash);
        tile.conn.active = true;
        // Never touched: the disabled gate returns before the connector is reached.
        tile.token = Some(Token(0));

        tile.on_top_bid(TopBidUpdate {
            timestamp: 1,
            slot: 5,
            block_number: 0,
            block_hash,
            parent_hash: B256::ZERO,
            builder_pubkey: BlsPublicKeyBytes::default(),
            fee_recipient: Address::ZERO,
            value: U256::ZERO,
        });

        assert!(tile.conn.activated.is_none());
        assert_eq!(tile.stats.activations_sent, 0);
    }

    fn reject(slot: u64, code: RejectCode) -> RejectV1 {
        RejectV1 {
            slot,
            code,
            subject: RejectSubject::BlockHash(B256::repeat_byte(7)),
            msg: b"test".to_vec(),
        }
    }

    fn top_bid(slot: u64, block_hash: B256) -> TopBidUpdate {
        TopBidUpdate {
            timestamp: 1,
            slot,
            block_number: 0,
            block_hash,
            parent_hash: B256::ZERO,
            builder_pubkey: BlsPublicKeyBytes::default(),
            fee_recipient: Address::ZERO,
            value: U256::ZERO,
        }
    }

    /// Sets up a tile mid-slot with a handshaken connection, ready to forward.
    fn tile_in_slot(bid_slot: u64) -> BlockMergingTile {
        let mut tile = test_tile(true);
        tile.slot.bid_slot = bid_slot;
        tile.slot.slot_start = Some(SlotStartV1 {
            slot: bid_slot,
            parent_hash: B256::ZERO,
            proposer_fee_recipient: Address::ZERO,
            parent_beacon_block_root: B256::ZERO,
        });
        tile.token = Some(Token(0));
        tile.conn.active = true;
        // `max_frame_bytes` stays 0, so a forward that gets past the guards stops at
        // the over-limits check and never reaches the connector, which has no real
        // socket in tests.
        tile
    }

    /// gattaca-com/helix#538: the merge builder validates against its own head, not
    /// our `SlotStartV1`. Once it has refused this slot as stale, everything else we
    /// send for the slot is refused too, so stop sending.
    #[test]
    fn stale_slot_reject_stops_forwarding_for_the_rest_of_the_slot() {
        let mut tile = tile_in_slot(5);
        tile.on_reject(Token(0), reject(5, RejectCode::StaleSlot));

        let block_hash = B256::repeat_byte(1);
        let ix = tile.decoded.push(test_submission(5, block_hash, true));
        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.is_empty(), "nothing should be forwarded");
        assert!(tile.slot.replay_log.is_empty());
        assert_eq!(tile.stats.skipped_builder_ahead, 1);
    }

    /// The builder's `RejectV1.slot` is its own current slot, so a reject naming a
    /// later slot proves we are behind whatever the code says. `tcp-types` does not
    /// document the field and helix's own builder puts the request's slot there, so
    /// both signals are honoured.
    #[test]
    fn reject_naming_a_later_slot_stops_forwarding() {
        let mut tile = tile_in_slot(5);
        tile.on_reject(Token(0), reject(6, RejectCode::Busy));

        let ix = tile.decoded.push(test_submission(5, B256::repeat_byte(2), true));
        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.is_empty());
        assert_eq!(tile.stats.skipped_builder_ahead, 1);
    }

    /// A reject for the current slot that is not about staleness says nothing about
    /// the builder's head, so forwarding must continue.
    #[test]
    fn other_reject_codes_for_the_current_slot_do_not_stop_forwarding() {
        let mut tile = tile_in_slot(5);
        tile.on_reject(Token(0), reject(5, RejectCode::Busy));
        tile.on_reject(Token(0), reject(5, RejectCode::InvalidOrder));

        let block_hash = B256::repeat_byte(3);
        let ix = tile.decoded.push(test_submission(5, block_hash, true));
        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.contains(&block_hash));
        assert_eq!(tile.stats.skipped_builder_ahead, 0);
    }

    /// A reject from a connection that is not ours must not gate our forwarding.
    #[test]
    fn reject_from_another_token_is_ignored() {
        let mut tile = tile_in_slot(5);
        tile.on_reject(Token(1), reject(5, RejectCode::StaleSlot));

        let block_hash = B256::repeat_byte(4);
        let ix = tile.decoded.push(test_submission(5, block_hash, true));
        tile.forward_decoded(ix, None);

        assert!(tile.slot.appendable.contains(&block_hash));
        assert_eq!(tile.stats.skipped_builder_ahead, 0);
    }

    /// Activating a base block while the builder is past our slot is what produces
    /// the `UnknownBaseBlock` rejects downstream, so it must stop too.
    #[test]
    fn stale_slot_reject_stops_activation() {
        let mut tile = tile_in_slot(5);
        let block_hash = B256::repeat_byte(5);
        tile.slot.appendable.insert(block_hash);
        tile.conn.forwarded.insert(block_hash);

        tile.on_reject(Token(0), reject(5, RejectCode::StaleSlot));
        tile.on_top_bid(top_bid(5, block_hash));

        assert!(tile.conn.activated.is_none());
        assert_eq!(tile.stats.activations_sent, 0);
    }

    /// A skipped forward must still feed the hydration cache, exactly as the
    /// neighbouring `skipped_no_slot_start` and `skipped_wrong_slot` guards do:
    /// dropping the frame must not cost later submissions their tx references.
    /// Testable since gattaca-com/helix#547 landed the submission helpers.
    #[test]
    fn a_suppressed_forward_still_feeds_the_hydration_cache() {
        let tx = full_tx_for_test(1);
        let dehydrated = dehydrated_submission_with_txs_for_test(vec![tx]);
        // The helper randomises the slot, so match the tile to it rather than
        // tripping the wrong-slot guard, which feeds the cache for its own reasons.
        let bid_slot = dehydrated.slot();

        let mut tile = tile_in_slot(bid_slot);
        tile.on_reject(Token(0), reject(bid_slot, RejectCode::StaleSlot));

        let mut data = test_submission(bid_slot, B256::repeat_byte(8), true);
        data.submission_data.submission = Submission::Dehydrated(dehydrated);
        let ix = tile.decoded.push(data);
        tile.forward_decoded(ix, None);

        assert_eq!(tile.stats.skipped_builder_ahead, 1, "the skip under test must be the reason");
        assert_eq!(tile.stats.skipped_wrong_slot, 0);
        assert_eq!(tile.hydration_cache.tx_count(), 1, "the skip must still feed the cache");
    }

    /// The suppression is per slot: once we catch up, forwarding resumes.
    #[test]
    fn slot_reset_clears_the_suppression() {
        let mut conn = Conn::default();
        conn.builder_ahead = true;

        conn.reset_slot();

        assert!(!conn.builder_ahead);
    }

    fn reject_with(slot: u64, code: RejectCode, subject: RejectSubject) -> RejectV1 {
        RejectV1 { slot, code, subject, msg: b"test".to_vec() }
    }

    /// gattaca-com/helix#538: `conn.forwarded` records what we sent, not what the
    /// builder accepted, so a rejected block stayed activatable and the activation
    /// came back as `UnknownBaseBlock`.
    #[test]
    fn reject_naming_a_block_hash_stops_it_being_activated() {
        let mut tile = tile_in_slot(5);
        let block_hash = B256::repeat_byte(1);
        tile.slot.appendable.insert(block_hash);
        tile.conn.forwarded.insert(block_hash);

        // Not StaleSlot: this must work while the builder is still on our slot,
        // where the `builder_ahead` guard does not apply.
        tile.on_reject(
            Token(0),
            reject_with(5, RejectCode::InvalidBaseBlock, RejectSubject::BlockHash(block_hash)),
        );
        tile.on_top_bid(top_bid(5, block_hash));

        assert!(!tile.conn.builder_ahead, "this reject says nothing about the builder's head");
        assert!(tile.conn.activated.is_none());
        assert_eq!(tile.stats.activations_sent, 0);
    }

    /// A reject naming a block means the builder does not hold it, whatever the
    /// code says, so it must leave `forwarded`.
    #[test]
    fn reject_naming_a_block_hash_drops_only_that_block() {
        let mut tile = tile_in_slot(5);
        let rejected = B256::repeat_byte(2);
        let kept = B256::repeat_byte(3);
        tile.conn.forwarded.insert(rejected);
        tile.conn.forwarded.insert(kept);

        tile.on_reject(
            Token(0),
            reject_with(5, RejectCode::InvalidOrder, RejectSubject::BlockHash(rejected)),
        );

        assert!(!tile.conn.forwarded.contains(&rejected));
        assert!(tile.conn.forwarded.contains(&kept));
    }

    /// `NotSynced` is documented as resendable, and a resend re-inserts the hash
    /// when it is forwarded again.
    #[test]
    fn a_block_dropped_by_a_reject_is_restored_by_forwarding_it_again() {
        let mut tile = tile_in_slot(5);
        tile.conn.max_frame_bytes = u32::MAX;
        tile.conn.max_orders_per_slot = u32::MAX;
        let block_hash = B256::repeat_byte(4);

        let ix = tile.decoded.push(test_submission(5, block_hash, true));
        tile.forward_decoded(ix, None);
        assert!(tile.conn.forwarded.contains(&block_hash));

        tile.on_reject(
            Token(0),
            reject_with(5, RejectCode::NotSynced, RejectSubject::BlockHash(block_hash)),
        );
        assert!(!tile.conn.forwarded.contains(&block_hash));

        tile.forward_decoded(ix, None);
        assert!(tile.conn.forwarded.contains(&block_hash));
    }

    /// An order-level reject is not about the base block, and a subjectless one
    /// names nothing, so neither may touch `forwarded`.
    #[test]
    fn rejects_without_a_block_subject_leave_forwarded_alone() {
        let mut tile = tile_in_slot(5);
        let block_hash = B256::repeat_byte(5);
        tile.conn.forwarded.insert(block_hash);

        tile.on_reject(
            Token(0),
            reject_with(
                5,
                RejectCode::InvalidOrder,
                RejectSubject::OrderHash(B256::repeat_byte(9)),
            ),
        );
        tile.on_reject(Token(0), reject_with(5, RejectCode::Busy, RejectSubject::None(0)));

        assert!(tile.conn.forwarded.contains(&block_hash));
    }

    /// State for our connection must not be edited by another connection's reject.
    #[test]
    fn reject_from_another_token_leaves_forwarded_alone() {
        let mut tile = tile_in_slot(5);
        let block_hash = B256::repeat_byte(6);
        tile.conn.forwarded.insert(block_hash);

        tile.on_reject(
            Token(1),
            reject_with(5, RejectCode::InvalidBaseBlock, RejectSubject::BlockHash(block_hash)),
        );

        assert!(tile.conn.forwarded.contains(&block_hash));
    }

    fn raw_plain_tx() -> Bytes {
        use alloy_consensus::TxEip1559;
        use alloy_primitives::Signature;
        use alloy_rlp::Encodable;

        let envelope = alloy_consensus::TxEnvelope::new_unhashed(
            TxEip1559::default().into(),
            Signature::new(Default::default(), Default::default(), Default::default()),
        );
        let mut raw = vec![];
        envelope.encode(&mut raw);
        raw.into()
    }

    fn test_merged_block(bid_slot: u64, base_block_hash: B256) -> MergedBlockV1 {
        let execution_payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::default(),
                    prev_randao: B256::ZERO,
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: Default::default(),
                    base_fee_per_gas: U256::from(1),
                    block_hash: base_block_hash,
                    // The base block's own payment tx plus the trailing distribution tx --
                    // every real merged block has at least these two (see `MergeSession::emit`).
                    transactions: vec![raw_plain_tx(), raw_plain_tx()],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        MergedBlockV1 {
            slot: bid_slot,
            response_id: 0,
            base_block_hash,
            base_builder_pubkey: BlsPublicKey::default(),
            execution_payload,
            execution_requests: ExecutionRequestsV4::default(),
            appended_blobs: vec![],
            proposer_value: U256::from(1),
            base_builder_revenue: U256::ZERO,
            relay_revenue: U256::ZERO,
            builder_inclusions: vec![],
            included_order_ids: vec![],
            trace: MergeTraceV1::default(),
        }
    }

    #[test]
    fn handle_merged_block_dropped_when_disabled() {
        let base_block_hash = B256::repeat_byte(9);
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        slot.appendable.insert(base_block_hash);
        let mut stats = SlotStats::default();
        let blob_sidecars = FxHashMap::default();
        let mut tx_hash_cache = FxHashMap::default();
        let mut bundled_scratch = Vec::new();
        let mut covered_scratch = Vec::new();

        let result = handle_merged_block(
            false,
            Token(0),
            test_merged_block(5, base_block_hash),
            &mut slot,
            &mut stats,
            &blob_sidecars,
            &mut tx_hash_cache,
            &mut bundled_scratch,
            &mut covered_scratch,
            9,
        );

        assert!(result.is_none());
        assert_eq!(stats.merged_blocks, 0);
    }

    #[test]
    fn handle_merged_block_accepted_when_enabled() {
        let base_block_hash = B256::repeat_byte(9);
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        slot.appendable.insert(base_block_hash);
        let mut stats = SlotStats::default();
        let blob_sidecars = FxHashMap::default();
        let mut tx_hash_cache = FxHashMap::default();
        let mut bundled_scratch = Vec::new();
        let mut covered_scratch = Vec::new();

        let result = handle_merged_block(
            true,
            Token(0),
            test_merged_block(5, base_block_hash),
            &mut slot,
            &mut stats,
            &blob_sidecars,
            &mut tx_hash_cache,
            &mut bundled_scratch,
            &mut covered_scratch,
            9,
        );

        assert!(result.is_some());
        assert_eq!(stats.merged_blocks, 1);
    }

    #[test]
    fn merge_sim_disable_check_table() {
        let mut rng = SmallRng::seed_from_u64(3);
        let payload = ExecutionPayload::random_for_test(&mut rng);
        let merged_blocks = SharedVector::<BlockMergeResponse>::with_capacity(4);
        let ix = merged_blocks.push(merge_response(payload, U256::from(1u64), vec![]));

        let cases: &[(BlockSimError, bool)] = &[
            (BlockSimError::RpcError, false),
            (BlockSimError::Timeout, false),
            (BlockSimError::NoSimulatorAvailable, false),
            (BlockSimError::SendError, false),
            (BlockSimError::SimulationDropped, false),
            (BlockSimError::HydrationMiss, false),
            (BlockSimError::BlockValidationFailed("unknown ancestor".to_owned()), false),
            (BlockSimError::BlockValidationFailed("parent block not found".to_owned()), false),
            (BlockSimError::BlockValidationFailed("block requires a reorg".to_owned()), false),
            (BlockSimError::BlockValidationFailed("block already known".to_owned()), false),
            (
                BlockSimError::BlockValidationFailed(
                    "block is too old, outside validation window".to_owned(),
                ),
                false,
            ),
            (
                BlockSimError::BlockValidationFailed("some other validation failure".to_owned()),
                true,
            ),
            (
                BlockSimError::InvalidTxRoot { got: B256::ZERO, expected: B256::repeat_byte(1) },
                true,
            ),
        ];

        for (err, expect_disable) in cases {
            let inner =
                MergedSimulationResultInner { merged_block_ix: ix, result: Err(err.clone()) };
            let outcome = merge_sim_disable_check(&inner, &merged_blocks);
            assert_eq!(outcome.is_some(), *expect_disable, "case: {err:?}");
        }
    }

    #[test]
    fn merge_sim_disable_check_reports_block_hash() {
        let mut rng = SmallRng::seed_from_u64(4);
        let payload = ExecutionPayload::random_for_test(&mut rng);
        let merged_blocks = SharedVector::<BlockMergeResponse>::with_capacity(4);
        let ix = merged_blocks.push(merge_response(payload.clone(), U256::from(1u64), vec![]));

        let inner = MergedSimulationResultInner {
            merged_block_ix: ix,
            result: Err(BlockSimError::InvalidTxRoot {
                got: B256::ZERO,
                expected: B256::repeat_byte(1),
            }),
        };
        let (block_hash, err) = merge_sim_disable_check(&inner, &merged_blocks).unwrap();
        assert_eq!(block_hash, payload.block_hash);
        assert!(matches!(err, BlockSimError::InvalidTxRoot { .. }));
    }

    #[test]
    fn merge_sim_disable_check_none_on_success() {
        let merged_blocks = SharedVector::<BlockMergeResponse>::with_capacity(4);
        let ix = merged_blocks.push(merge_response(
            ExecutionPayload::random_for_test(&mut SmallRng::seed_from_u64(5)),
            U256::ZERO,
            vec![],
        ));
        let inner = MergedSimulationResultInner { merged_block_ix: ix, result: Ok(()) };
        assert!(merge_sim_disable_check(&inner, &merged_blocks).is_none());
    }

    /// RELAY-FR: when this slot has no cached beacon payload attributes for the merged
    /// block's own parent hash, the request must be skipped rather than silently carry
    /// a zero `parent_beacon_block_root`. EIP-4788 writes this value into the
    /// beacon-roots contract during execution, so sending zero when the real root is
    /// non-zero produces a genuine state-root mismatch downstream ("invalid merkle
    /// root").
    #[test]
    fn merged_validation_request_none_when_attrs_missing() {
        let slot = SlotState { bid_slot: 5, ..Default::default() }; // attrs empty
        let base_block_hash = B256::repeat_byte(1);
        let parent_hash = B256::repeat_byte(2);

        let req = merged_validation_request(base_block_hash, parent_hash, &slot, 0, 0);

        assert!(req.is_none(), "must not silently send a zero beacon root when it's unknown");
    }

    /// RELAY-FR: `proposer_fee_recipient` must not silently default to the zero
    /// address when this slot's registered fee recipient isn't known yet -- the
    /// external validator checks the merge builder's payment tx against it, so a
    /// zero-defaulted recipient produces "could not verify proposer payment" for a
    /// perfectly valid block.
    #[test]
    fn merged_validation_request_none_when_fee_recipient_missing() {
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        let base_block_hash = B256::repeat_byte(1);
        let parent_hash = B256::repeat_byte(2);
        slot.attrs.insert(parent_hash, B256::repeat_byte(3));
        slot.registered_gas_limit = Some(30_000_000);
        // fee_recipient left unset

        let req = merged_validation_request(base_block_hash, parent_hash, &slot, 0, 0);

        assert!(req.is_none(), "must not silently send a zero fee recipient when it's unknown");
    }

    /// Same silent-default hazard as the fee recipient: a gas limit of zero would
    /// simulate a merged block against the wrong registered limit for this proposer.
    #[test]
    fn merged_validation_request_none_when_gas_limit_missing() {
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        let base_block_hash = B256::repeat_byte(1);
        let parent_hash = B256::repeat_byte(2);
        slot.attrs.insert(parent_hash, B256::repeat_byte(3));
        slot.fee_recipient = Some(alloy_primitives::Address::repeat_byte(4));
        // registered_gas_limit left unset

        let req = merged_validation_request(base_block_hash, parent_hash, &slot, 0, 0);

        assert!(req.is_none(), "must not silently send a zero gas limit when it's unknown");
    }

    #[test]
    fn merged_validation_request_uses_known_slot_fields() {
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        let base_block_hash = B256::repeat_byte(1);
        let parent_hash = B256::repeat_byte(2);
        let expected_root = B256::repeat_byte(3);
        let expected_fee_recipient = alloy_primitives::Address::repeat_byte(4);
        slot.attrs.insert(parent_hash, expected_root);
        slot.fee_recipient = Some(expected_fee_recipient);
        slot.registered_gas_limit = Some(30_000_000);

        let req = merged_validation_request(base_block_hash, parent_hash, &slot, 7, 42)
            .expect("known fields resolve");

        assert_eq!(req.parent_beacon_block_root, expected_root);
        assert_eq!(req.proposer_fee_recipient, expected_fee_recipient);
        assert_eq!(req.registered_gas_limit, 30_000_000);
        assert_eq!(req.base_block_hash, base_block_hash);
        assert_eq!(req.merged_block_ix, 7);
        assert_eq!(req.receive_ns, 42);
    }
}
