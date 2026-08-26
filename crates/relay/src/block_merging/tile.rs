use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
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
    builder_to_relay::{FatalV1, MergedBlockV1, RejectV1},
    control::{
        BuilderCollateral, MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1,
    },
    order::{MergeOrderRef, order_id},
    relay_to_builder::{ActivateBaseBlockV1, MergeableBlockV1, RevokeOrderV1, SlotStartV1},
};
use helix_types::{
    BlobWithMetadata, BlsPublicKeyBytes, BuilderInclusionResult, HydrationCache, Submission,
    payload_to_v3,
};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Decode;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::{
    HelixSpine, SimRequest, SimResult, SubmissionDataWithSpan,
    block_merging::{
        append_frame, merged_block_to_response, order_ref_hash, order_to_ref,
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
    /// base_block_hash -> best proposer_value and the order_ids it was built from.
    best_merged: FxHashMap<B256, BestMergedFloor>,
    /// Orders actually sent to the merge builder this slot, for the
    /// unbundling check on incoming merged blocks.
    order_txs: Vec<OrderTxs>,
    /// Per-builder map of order_id -> order_hash for their latest_only bundles this
    /// slot, diffed on each new submission to detect revocations.
    latest_only_ids: FxHashMap<BlsPublicKeyBytes, FxHashMap<B256, B256>>,
}

#[derive(Clone, Copy)]
enum ReplayEvent {
    Forward(usize),
    Revoke { order_hash: B256, builder_pubkey: BlsPublicKeyBytes },
}

#[derive(Default)]
struct BestMergedFloor {
    value: U256,
    /// order_ids of the merged block currently holding this floor.
    order_ids: FxHashSet<B256>,
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
    /// Merged blocks dropped because an appended blob's sidecar wasn't in our cache.
    merged_blob_missing: usize,
    /// Merged blocks whose simulation was skipped because this slot's beacon parent
    /// root for the merged block's parent hash isn't known yet.
    merged_root_missing: usize,
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
    // Builders only guarantee monotonicity within a connection; filter
    // so the stored merged bid never regresses.
    if slot
        .best_merged
        .get(&merged.base_block_hash)
        .is_some_and(|floor| merged.proposer_value <= floor.value)
    {
        stats.merged_regressed += 1;
        return None;
    }
    slot.best_merged.insert(merged.base_block_hash, BestMergedFloor {
        value: merged.proposer_value,
        order_ids: merged.included_order_ids.iter().copied().collect(),
    });
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
/// `parent_beacon_block_root` from this slot's cached beacon payload attributes
/// (`slot.attrs`, keyed by the merged block's own parent hash). `None` if that root
/// isn't known yet: EIP-4788 writes it into the beacon-roots contract during
/// execution, so simulating with a wrong (e.g. zero-defaulted) root would produce a
/// genuine state-root mismatch that isn't actually the merge builder's fault.
fn merged_validation_request(
    base_block_hash: B256,
    parent_hash: B256,
    slot: &SlotState,
    merged_block_ix: usize,
    receive_ns: u64,
) -> Option<MergedValidationRequest> {
    let parent_beacon_block_root = *slot.attrs.get(&parent_hash)?;
    Some(MergedValidationRequest {
        merged_block_ix,
        base_block_hash,
        slot: slot.bid_slot,
        parent_beacon_block_root,
        proposer_fee_recipient: slot.fee_recipient.unwrap_or_default(),
        registered_gas_limit: slot.registered_gas_limit.unwrap_or_default(),
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

/// order_id -> order_hash for every `latest_only` bundle in this submission.
fn latest_only_ids(
    builder_pubkey: BlsPublicKeyBytes,
    merge_orders: &[MergeOrderRef],
    order_hashes: &[B256],
) -> FxHashMap<B256, B256> {
    merge_orders
        .iter()
        .zip(order_hashes)
        .filter(|(order_ref, _)| matches!(order_ref, MergeOrderRef::Bundle(b) if b.latest_only))
        .map(|(_, &hash)| (order_id(hash, &builder_pubkey), hash))
        .collect()
}

/// (order_id, order_hash) pairs present in `prev` but missing from `new` —
/// i.e. flagged latest_only before, dropped from the newest submission.
fn revoked_ids(
    prev: Option<&FxHashMap<B256, B256>>,
    new: &FxHashMap<B256, B256>,
) -> Vec<(B256, B256)> {
    let Some(prev) = prev else { return Vec::new() };
    prev.iter().filter(|(id, _)| !new.contains_key(*id)).map(|(&id, &hash)| (id, hash)).collect()
}

/// Tx hashes the merge builder actually appended onto the base block.
/// `builder_inclusions` only ever records orders that were applied (see
/// `record_inclusion` on the builder side), so this never includes anything
/// from the base block's own original content — which is what the
/// unbundling check must ignore, since orders sharing a tx hash with base
/// content that was never touched by the merge builder aren't its concern.
fn appended_tx_hashes(
    builder_inclusions: &HashMap<Address, BuilderInclusionResult>,
) -> FxHashSet<B256> {
    builder_inclusions.values().flat_map(|inclusion| inclusion.txs.iter().copied()).collect()
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
                    ReplayEvent::Revoke { order_hash, builder_pubkey } => {
                        let msg =
                            RevokeOrderV1 { slot: self.slot.bid_slot, order_hash, builder_pubkey };
                        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                            append_frame(buf, MergingMsgId::RevokeOrderV1, &msg);
                        });
                    }
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
                                    stats.merged_root_missing += 1;
                                    warn!(
                                        ?token,
                                        %parent_hash,
                                        "no cached beacon parent root for merged block's \
                                         parent hash, skipping simulation"
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
            skipped_over_limits = stats.skipped_over_limits,
            orders_dropped = stats.orders_dropped,
            replayed = stats.replayed,
            merged_blocks = stats.merged_blocks,
            merged_stale = stats.merged_stale,
            merged_regressed = stats.merged_regressed,
            merged_blob_missing = stats.merged_blob_missing,
            merged_root_missing = stats.merged_root_missing,
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
        let Some(merging) = &sub.merging_data else {
            self.stats.skipped_no_merging_data += 1;
            // No merging data revokes everything this builder previously flagged.
            if !is_replay {
                self.diff_latest_only(*sub.submission.builder_pubkey(), &[], &[]);
            }
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

        if !is_replay {
            self.diff_latest_only(signed.message.builder_pubkey, &msg.merge_orders, &order_hashes);
        }

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

    /// Diffs `builder_pubkey`'s latest_only bundles against their previous
    /// submission this slot to detect revocations.
    fn diff_latest_only(
        &mut self,
        builder_pubkey: BlsPublicKeyBytes,
        merge_orders: &[MergeOrderRef],
        order_hashes: &[B256],
    ) {
        let new_ids = latest_only_ids(builder_pubkey, merge_orders, order_hashes);
        let prev_ids = self.slot.latest_only_ids.get(&builder_pubkey);
        for (revoked_id, revoked_hash) in revoked_ids(prev_ids, &new_ids) {
            self.revoke_order(revoked_id, revoked_hash, builder_pubkey);
        }
        self.slot.latest_only_ids.insert(builder_pubkey, new_ids);
    }

    /// Relaxes the floor for any base block that depended on `order_id`, notifies
    /// the auctioneer to evict cached bids, and tells the merge builder to drop it.
    fn revoke_order(
        &mut self,
        order_id: B256,
        order_hash: B256,
        builder_pubkey: BlsPublicKeyBytes,
    ) {
        self.slot.best_merged.retain(|_, floor| !floor.order_ids.contains(&order_id));

        let bid_slot = self.slot.bid_slot;

        self.slot.replay_log.push(ReplayEvent::Revoke { order_hash, builder_pubkey });
        if let Some(token) = self.token &&
            self.conn.active
        {
            let msg = RevokeOrderV1 { slot: bid_slot, order_hash, builder_pubkey };
            self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
                append_frame(buf, MergingMsgId::RevokeOrderV1, &msg);
            });
        }
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
    use helix_tcp_types::{
        MergeType,
        merging::{
            builder_to_relay::MergeTraceV1,
            order::{BundleOrderRef, TxOrderRef},
        },
    };
    use helix_types::{
        BlobsBundle, BlockMergingData, Compression, ExecutionPayload, ExecutionRequests, ForkName,
        MergedBlockTrace, SignedBidSubmission, SubmissionVersion, TestRandom, TestRandomSeed,
    };
    use rand::{SeedableRng, rngs::SmallRng};

    use super::*;
    use crate::{SubmissionRef, auctioneer::SubmissionData};

    fn bundle(latest_only: bool) -> MergeOrderRef {
        MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![0],
            reverting_txs: vec![],
            dropping_txs: vec![],
            latest_only,
        })
    }

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
            trace: MergedBlockTrace::default(),
        }
    }

    #[test]
    fn latest_only_ids_ignores_non_flagged_and_tx_orders() {
        let builder_pubkey = BlsPublicKeyBytes::default();
        let orders = [
            bundle(true),
            bundle(false),
            MergeOrderRef::Tx(TxOrderRef { index: 0, can_revert: false }),
        ];
        let hashes = [B256::repeat_byte(1), B256::repeat_byte(2), B256::repeat_byte(3)];

        let ids = latest_only_ids(builder_pubkey, &orders, &hashes);

        assert_eq!(ids.len(), 1);
        assert_eq!(*ids.values().next().unwrap(), hashes[0]);
    }

    #[test]
    fn latest_only_ids_disambiguates_by_builder() {
        let hash = [B256::repeat_byte(9)];
        let orders = [bundle(true)];
        let mut pubkey_b = [0u8; 48];
        pubkey_b[0] = 1;

        let ids_a = latest_only_ids(BlsPublicKeyBytes::default(), &orders, &hash);
        let ids_b = latest_only_ids(BlsPublicKeyBytes::from(pubkey_b), &orders, &hash);

        assert_ne!(ids_a.keys().next(), ids_b.keys().next());
    }

    #[test]
    fn revoked_ids_detects_a_dropped_flag() {
        let builder_pubkey = BlsPublicKeyBytes::default();
        let hash = B256::repeat_byte(5);
        let prev = latest_only_ids(builder_pubkey, &[bundle(true)], &[hash]);

        // Resubmission without the bundle at all.
        let new = latest_only_ids(builder_pubkey, &[], &[]);

        let revoked = revoked_ids(Some(&prev), &new);
        assert_eq!(revoked, vec![(*prev.keys().next().unwrap(), hash)]);
    }

    #[test]
    fn revoked_ids_empty_when_still_present() {
        let builder_pubkey = BlsPublicKeyBytes::default();
        let hash = [B256::repeat_byte(6)];
        let orders = [bundle(true)];

        let prev = latest_only_ids(builder_pubkey, &orders, &hash);
        let new = latest_only_ids(builder_pubkey, &orders, &hash);

        assert!(revoked_ids(Some(&prev), &new).is_empty());
    }

    #[test]
    fn revoked_ids_empty_on_first_submission() {
        let new =
            latest_only_ids(BlsPublicKeyBytes::default(), &[bundle(true)], &[B256::repeat_byte(7)]);
        assert!(revoked_ids(None, &new).is_empty());
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
                    transactions: vec![],
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
        assert!(slot.best_merged.is_empty());
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
        assert!(slot.best_merged.contains_key(&base_block_hash));
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

    #[test]
    fn merged_validation_request_uses_known_parent_beacon_block_root() {
        let mut slot = SlotState { bid_slot: 5, ..Default::default() };
        let base_block_hash = B256::repeat_byte(1);
        let parent_hash = B256::repeat_byte(2);
        let expected_root = B256::repeat_byte(3);
        slot.attrs.insert(parent_hash, expected_root);

        let req = merged_validation_request(base_block_hash, parent_hash, &slot, 7, 42)
            .expect("known root resolves");

        assert_eq!(req.parent_beacon_block_root, expected_root);
        assert_eq!(req.base_block_hash, base_block_hash);
        assert_eq!(req.merged_block_ix, 7);
        assert_eq!(req.receive_ns, 42);
    }
}
