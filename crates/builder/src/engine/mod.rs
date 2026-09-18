//! Merge engine: consumes decoded protocol events from the TCP server tile
//! over a bounded channel, maintains the per-slot order pool and merge
//! session, and streams merged blocks / rejects back. Everything
//! ethrex-related stays behind this boundary.

pub mod convert;
pub mod error;
pub mod payment;
pub mod session;
pub mod simulate;
pub mod streams;
#[cfg(test)]
mod tests;
pub mod types;

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use alloy_primitives::{B256, keccak256};
use crossbeam_channel::{Receiver, Sender};
use ethrex_blockchain::Blockchain;
use ethrex_crypto::native::NativeCrypto;
use ethrex_storage::Store;
use helix_tcp_types::merging::{
    builder_to_relay::{MergedBlockV1, RejectCode, RejectSubject, RejectV1},
    control::RelayConfigV1,
    order::{
        MAX_BLOCK_TXS, MAX_ORDERS_PER_BLOCK, MAX_TX_BYTES, MergeOrderRef, OrderMeta,
        bundle_order_hash, is_tx_hash_ref,
    },
    relay_to_builder::{MergeableBlockV1, SlotStartV1},
};
use ssz::Decode;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::{
    engine::{
        error::MergeError,
        streams::{MergeStreams, Offer, StreamJob},
        types::{
            DecodedTx, EngineConfig, PreparedBlock, PreparedOrder, SharedInner, SharedSlot,
            SlotState,
        },
    },
    metrics,
    node::HeadInfo,
};

/// Events from the server tile. `generation` identifies the relay connection
/// that produced the event: it bumps every time the active connection is
/// replaced, and outputs carrying a stale generation are dropped by the tile.
pub enum EngineEvent {
    /// The active relay connection was replaced or lost: drop per-connection
    /// emission bookkeeping and expect a full slot replay.
    ConnectionReset {
        generation: u64,
    },
    /// Distribution policy; takes effect at the next `SlotStart`.
    RelayConfig(RelayConfigV1),
    SlotStart(SlotStartV1),
    SlotEnd {
        slot: u64,
    },
    /// Raw SSZ body of a `MergeableBlockV1` (already decompressed). The engine
    /// does the SSZ + transaction decoding off the tile thread.
    MergeableBlock {
        body: Vec<u8>,
        recv_ns: u64,
        generation: u64,
    },
    ActivateBase {
        slot: u64,
        block_hash: B256,
        generation: u64,
    },
}

/// Outputs to the server tile. `response_id` on `MergedBlockV1` is left 0; the
/// tile stamps its per-connection monotonic id right before the frame is sent.
pub enum EngineOutput {
    Merged { generation: u64, msg: Box<MergedBlockV1> },
    Reject { generation: u64, msg: RejectV1 },
}

impl EngineOutput {
    pub fn generation(&self) -> u64 {
        match self {
            EngineOutput::Merged { generation, .. } => *generation,
            EngineOutput::Reject { generation, .. } => *generation,
        }
    }

    pub fn reject(
        generation: u64,
        slot: u64,
        code: RejectCode,
        subject: RejectSubject,
        msg: impl Into<String>,
    ) -> Self {
        EngineOutput::Reject {
            generation,
            msg: RejectV1 { slot, code, subject, msg: msg.into().into_bytes() },
        }
    }
}

/// The ethrex-backed merge engine worker.
pub struct MergeEngine {
    config: Arc<EngineConfig>,
    head: watch::Receiver<HeadInfo>,
    out: Sender<EngineOutput>,
    /// Latest connection generation; events from older connections are dropped.
    generation: u64,
    /// Latest relay config; snapshotted into the slot at `SlotStart`.
    relay_config: Option<Arc<RelayConfigV1>>,
    slot: Option<SlotState>,
    /// Per-builder merge streams; `None` when disabled.
    streams: Option<MergeStreams>,
}

impl MergeEngine {
    pub fn spawn(
        config: EngineConfig,
        store: Store,
        blockchain: Arc<Blockchain>,
        head: watch::Receiver<HeadInfo>,
        event_tx: Sender<EngineEvent>,
        events: Receiver<EngineEvent>,
        out: Sender<EngineOutput>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("merge-engine".into())
            .spawn(move || {
                if let Some(core) = config.core &&
                    !core_affinity::set_for_current(core_affinity::CoreId { id: core })
                {
                    warn!(core, "failed to pin merge engine thread");
                }
                let config = Arc::new(config);
                let streams = (config.max_builder_streams > 0).then(|| {
                    MergeStreams::new(
                        config.max_builder_streams,
                        &config.replay_worker_cores,
                        store.clone(),
                        blockchain.clone(),
                        out.clone(),
                    )
                });
                drop(event_tx);
                let mut engine = MergeEngine {
                    config,
                    head,
                    out,
                    generation: 0,
                    relay_config: None,
                    slot: None,
                    streams,
                };
                info!("merge engine started");
                engine.run(events);
                info!("merge engine stopped");
            })
            .expect("failed to spawn merge engine thread")
    }

    fn run(&mut self, events: Receiver<EngineEvent>) {
        while let Ok(event) = events.recv() {
            self.handle_event(event);
            while let Ok(event) = events.try_recv() {
                self.handle_event(event);
            }
        }
    }

    fn teardown_slot(&mut self, reason: &str) {
        if let Some(state) = self.slot.take() {
            let (builders, orders, top_rise, top_span) = match &state.shared {
                Some(shared) => {
                    let inner = shared.inner.read().expect("shared slot poisoned");
                    let top = inner.top_builders(1).first().and_then(|b| inner.submissions.get(b));
                    (
                        inner.submissions.len(),
                        inner.orders.len(),
                        top.map(|s| metrics::gwei(s.total_rise)).unwrap_or_default(),
                        top.map(|s| s.span_ms()).unwrap_or_default(),
                    )
                }
                None => (0, 0, 0.0, 0),
            };
            info!(
                reason,
                slot = state.slot,
                blocks_pooled = state.blocks.len(),
                orders_pooled = orders,
                offered = state.stream.offered,
                superseded = state.stream.superseded,
                refused = state.stream.refused,
                skipped_not_top_k = state.stream.skipped_not_top_k,
                builders,
                top_builder_rise_gwei = top_rise,
                top_builder_span_ms = top_span,
                "merge slot ended"
            );
            metrics::slot_pool(state.blocks.len(), orders, builders);
        }
    }

    /// Applies one event to the engine state; returns whether a merge pass is
    /// warranted afterwards.
    fn handle_event(&mut self, event: EngineEvent) -> bool {
        match event {
            EngineEvent::ConnectionReset { generation } => {
                debug!(generation, "connection reset, dropping slot state");
                self.generation = generation;
                self.teardown_slot("connection_reset");
                false
            }
            EngineEvent::RelayConfig(config) => {
                info!(
                    relay_bps = config.relay_bps,
                    merged_builder_bps = config.merged_builder_bps,
                    winning_builder_bps = config.winning_builder_bps,
                    collaterals = config.builder_collaterals.len(),
                    "relay config received"
                );
                self.relay_config = Some(Arc::new(config));
                false
            }
            EngineEvent::SlotStart(msg) => {
                if let Some(slot) = &self.slot {
                    if msg.slot < slot.slot {
                        return false;
                    }
                    if msg.slot == slot.slot && msg.parent_hash == slot.parent_hash {
                        // Idempotent re-send (e.g. handshake replay).
                        return false;
                    }
                }
                debug!(slot = msg.slot, parent_hash = %msg.parent_hash, "slot start");
                self.teardown_slot("slot_start");
                let mut state = SlotState::new(&msg);
                state.shared = self.relay_config.clone().map(|relay_config| {
                    Arc::new(SharedSlot {
                        ctx: state.ctx.clone(),
                        relay_config,
                        engine_config: self.config.clone(),
                        inner: RwLock::new(SharedInner::default()),
                        pool_version: AtomicU64::new(0),
                    })
                });
                self.slot = Some(state);
                false
            }
            EngineEvent::SlotEnd { slot } => {
                if self.slot.as_ref().is_some_and(|s| s.slot == slot) {
                    debug!(slot, "slot end");
                    self.teardown_slot("slot_end");
                }
                false
            }
            EngineEvent::MergeableBlock { body, recv_ns, generation } => {
                if generation != self.generation {
                    return false;
                }
                match self.ingest_mergeable_block(&body, recv_ns) {
                    Ok(()) => true,
                    Err((slot, block_hash, err)) => {
                        metrics::rejection("ingest", err.metric_label());
                        warn!(%err, "mergeable block rejected");
                        if let Some((code, subject)) = err.reject(block_hash) {
                            let _ = self.out.send(EngineOutput::reject(
                                self.generation,
                                slot,
                                code,
                                subject,
                                err.to_string(),
                            ));
                        }
                        false
                    }
                }
            }
            EngineEvent::ActivateBase { slot, block_hash, generation } => {
                if generation != self.generation {
                    return false;
                }
                let Some(state) = self.slot.as_mut() else { return false };
                if state.slot != slot {
                    let _unused = self.out.send(EngineOutput::reject(
                        self.generation,
                        slot,
                        RejectCode::StaleSlot,
                        RejectSubject::BlockHash(block_hash),
                        "activation for a slot that is not current",
                    ));
                    return false;
                }
                // Advisory only. We cannot know which builder holds the top bid
                // when get_header is called, so every candidate builder gets a
                // stream regardless; this just records the relay's current view.
                match state.blocks.get(&block_hash) {
                    Some(base) => {
                        let beneficiary = base.beneficiary();
                        state.top_builder = Some(beneficiary);
                        metrics::activation_source("advisory");
                    }
                    None => metrics::activation_source("unknown_base_block"),
                }
                false
            }
        }
    }

    /// Queues a speculative replay for a newly pooled appendable block.
    /// Offers a newly pooled appendable block to its builder's merge stream.
    /// Never blocks: a refusal just means that builder is not merged this round.
    fn offer_to_stream(&mut self, base: &Arc<PreparedBlock>) {
        let Some(streams) = self.streams.as_ref() else { return };
        let Some(state) = self.slot.as_mut() else { return };
        let Some(shared) = state.shared.clone() else { return };

        let beneficiary = base.beneficiary();
        let top_k = self.config.speculation_top_k;
        if top_k > 0 {
            let in_top_k = {
                let inner = shared.inner.read().expect("shared slot poisoned");
                inner.top_builders(top_k).contains(&beneficiary)
            };
            if !in_top_k && state.top_builder != Some(beneficiary) {
                state.stream.skipped_not_top_k += 1;
                metrics::speculation("skipped_not_top_k");
                return;
            }
        }

        let head = *self.head.borrow();
        if !head.is_synced || convert::b256(head.hash) != state.parent_hash {
            metrics::speculation("head_not_ready");
            return;
        }

        let job = StreamJob {
            shared,
            base: base.clone(),
            generation: self.generation,
            offered_ns: crate::utils::utcnow_ns(),
        };
        match streams.offer(beneficiary, job) {
            Offer::Accepted => {
                state.stream.offered += 1;
                metrics::speculation("offered");
            }
            Offer::Superseded => {
                state.stream.offered += 1;
                state.stream.superseded += 1;
                metrics::speculation("offered");
                metrics::speculation("superseded");
            }
            Offer::Refused => {
                state.stream.refused += 1;
                metrics::speculation("refused");
            }
        }
    }

    /// Decodes and pools a forwarded `MergeableBlockV1`.
    #[allow(clippy::type_complexity)]
    fn ingest_mergeable_block(
        &mut self,
        body: &[u8],
        recv_ns: u64,
    ) -> Result<(), (u64, Option<B256>, MergeError)> {
        let current_slot = self.slot.as_ref().map(|s| s.slot).unwrap_or_default();
        let msg = MergeableBlockV1::from_ssz_bytes(body).map_err(|e| {
            (current_slot, None, MergeError::InvalidOrder(format!("undecodable block: {e:?}")))
        })?;
        let block_hash = msg.execution_payload.payload_inner.payload_inner.block_hash;
        let fail = |err: MergeError| (msg.slot, Some(block_hash), err);

        let Some(state) = self.slot.as_mut() else {
            return Err(fail(MergeError::StaleSlot));
        };
        if msg.slot != state.slot {
            return Err(fail(MergeError::StaleSlot));
        }
        if msg.execution_payload.payload_inner.payload_inner.parent_hash != state.parent_hash {
            return Err(fail(MergeError::HeadMismatch));
        }
        if state.blocks.contains_key(&block_hash) {
            // Same block re-forwarded (e.g. handshake replay): nothing new.
            return Ok(());
        }
        let Some(shared) = state.shared.clone() else {
            return Err(fail(MergeError::StaleSlot));
        };
        let (submission_index, ratchet) = {
            let mut inner = shared.inner.write().expect("shared slot poisoned");
            inner.record_submission(msg.builder_address, msg.block_value, recv_ns)
        };
        if let Some((delta, interval_ms, rising)) = ratchet {
            metrics::ratchet(delta, interval_ms, rising);
        }
        if state.blocks.len() >= self.config.max_blocks_per_slot {
            return Err(fail(MergeError::LimitExceeded("max blocks per slot".into())));
        }
        let tx_bytes = &msg.execution_payload.payload_inner.payload_inner.transactions;
        if tx_bytes.len() > MAX_BLOCK_TXS {
            return Err(fail(MergeError::LimitExceeded("max block txs".into())));
        }
        if tx_bytes.iter().any(|tx| tx.len() > MAX_TX_BYTES) {
            return Err(fail(MergeError::InvalidOrder("oversized transaction".into())));
        }
        if msg.merge_orders.len() > MAX_ORDERS_PER_BLOCK {
            return Err(fail(MergeError::LimitExceeded("max orders per block".into())));
        }
        // Decode txs and recover senders in parallel, through the per-slot
        // cache (incremental submissions share most txs); resolves any
        // tx-hash references against txs already seen whole this slot. Runs before the
        // order checks so a block rejected for its orders still fills the tx cache.
        let decoded = decode_block_txs(&msg, &mut state.recovery_cache, &mut state.tx_cache)
            .map_err(|err| fail(MergeError::InvalidOrder(err)))?;

        for order in &msg.merge_orders {
            order
                .validate(tx_bytes.len())
                .map_err(|_| fail(MergeError::InvalidOrder("order ref out of range".into())))?;
        }
        let txs = Arc::new(decoded);

        let prepared_orders: Vec<PreparedOrder> = msg
            .merge_orders
            .iter()
            .map(|order_ref| prepare_order(&msg, order_ref, &txs, block_hash))
            .collect();

        // Budget counts distinct pooled orders, as the relay's `orders_sent` does.
        let mut pool_full = false;
        {
            let mut inner = shared.inner.write().expect("shared slot poisoned");
            inner.update_latest_only(
                msg.builder_pubkey,
                prepared_orders
                    .iter()
                    .filter(|order| order.latest_only)
                    .map(|order| order.order_hash)
                    .collect(),
            );
            for prepared in prepared_orders {
                match inner.order_ids.get(&prepared.order_id) {
                    Some(&existing_ix) => {
                        // Duplicate order: attribution goes to the highest-value
                        // source block.
                        let existing = &inner.orders[existing_ix];
                        if msg.block_value > existing.source_block_value {
                            inner.orders[existing_ix] = prepared;
                        }
                    }
                    None => {
                        if inner.orders.len() >= self.config.max_orders_per_slot {
                            pool_full = true;
                            break;
                        }
                        let ix = inner.orders.len();
                        inner.order_ids.insert(prepared.order_id, ix);
                        inner.orders.push(prepared);
                    }
                }
            }
        }

        shared.pool_version.fetch_add(1, Ordering::Release);
        if let Some(streams) = self.streams.as_ref() {
            streams.wake_all();
        }

        debug!(
            slot = msg.slot,
            %block_hash,
            txs = txs.len(),
            orders = msg.merge_orders.len(),
            pool = shared.inner.read().map(|i| i.orders.len()).unwrap_or_default(),
            allow_appending = msg.allow_appending,
            "mergeable block pooled"
        );

        let slot = state.slot;
        let prepared = Arc::new(PreparedBlock {
            block_hash,
            builder_pubkey: msg.builder_pubkey,
            block_value: msg.block_value,
            allow_appending: msg.allow_appending,
            payload: msg.execution_payload,
            txs,
            recv_ns,
            submission_index,
            ingest_done_ns: crate::utils::utcnow_ns(),
        });
        state.blocks.insert(block_hash, prepared.clone());

        // Every appendable base goes to its builder's stream.
        if prepared.allow_appending {
            self.offer_to_stream(&prepared);
        }

        // Dropped orders still leave a usable base candidate, so keep the block.
        if pool_full {
            let err = MergeError::LimitExceeded("max orders per slot".into());
            if let Some((code, subject)) = err.reject(Some(block_hash)) {
                let _ = self.out.send(EngineOutput::reject(
                    self.generation,
                    slot,
                    code,
                    subject,
                    err.to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Decodes every tx in the payload and recovers senders (cache-assisted, the
/// misses in parallel). Entries of `order::TX_HASH_REF_LEN` bytes are
/// tx-hash references rather than raw txs (see `MergeableBlockV1`'s doc
/// comment): resolved against `tx_cache`, which holds every tx already sent
/// whole on this connection this slot.
fn decode_block_txs(
    msg: &MergeableBlockV1,
    recovery_cache: &mut rustc_hash::FxHashMap<B256, ethrex_common::Address>,
    tx_cache: &mut rustc_hash::FxHashMap<B256, Arc<DecodedTx>>,
) -> Result<Vec<Arc<DecodedTx>>, String> {
    use rayon::prelude::*;

    let tx_bytes = &msg.execution_payload.payload_inner.payload_inner.transactions;

    struct Partial {
        tx: ethrex_common::types::Transaction,
        hash: B256,
        cached_sender: Option<ethrex_common::Address>,
    }

    enum Entry {
        Cached(Arc<DecodedTx>),
        New(Box<Partial>),
    }

    // Never short-circuit: this block's own txs must reach the caches even when one of its
    // references does not resolve, or every later block that references them fails too.
    let mut first_err: Option<String> = None;
    let entries: Vec<Option<Entry>> = tx_bytes
        .iter()
        .map(|bytes| {
            let entry = if is_tx_hash_ref(bytes) {
                let hash = B256::from_slice(bytes);
                tx_cache
                    .get(&hash)
                    .cloned()
                    .map(Entry::Cached)
                    .ok_or_else(|| format!("unresolved tx hash reference: {hash}"))
            } else {
                ethrex_common::types::Transaction::decode_canonical(bytes)
                    .map_err(|e| format!("tx decode: {e}"))
                    .map(|tx| {
                        let hash = keccak256(bytes);
                        Entry::New(Box::new(Partial {
                            tx,
                            hash,
                            cached_sender: recovery_cache.get(&hash).copied(),
                        }))
                    })
            };
            match entry {
                Ok(entry) => Some(entry),
                Err(err) => {
                    first_err.get_or_insert(err);
                    None
                }
            }
        })
        .collect();

    let decoded: Vec<Option<Result<Arc<DecodedTx>, String>>> = entries
        .into_par_iter()
        .map(|entry| {
            let partial = match entry? {
                Entry::Cached(tx) => return Some(Ok(tx)),
                Entry::New(partial) => *partial,
            };
            let sender = match partial.cached_sender {
                Some(sender) => sender,
                None => match partial.tx.sender(&NativeCrypto) {
                    Ok(sender) => sender,
                    Err(e) => return Some(Err(format!("sender recovery: {e}"))),
                },
            };
            let blob_hashes =
                partial.tx.blob_versioned_hashes().into_iter().map(convert::b256).collect();
            Some(Ok(Arc::new(DecodedTx {
                gas_limit: partial.tx.gas_limit(),
                blob_hashes,
                hash: partial.hash,
                sender,
                tx: partial.tx,
            })))
        })
        .collect();

    let mut txs = Vec::with_capacity(decoded.len());
    for entry in decoded {
        match entry {
            Some(Ok(tx)) => {
                recovery_cache.insert(tx.hash, tx.sender);
                tx_cache.entry(tx.hash).or_insert_with(|| tx.clone());
                txs.push(tx);
            }
            Some(Err(err)) => {
                first_err.get_or_insert(err);
            }
            None => {}
        }
    }

    match first_err {
        Some(err) => Err(err),
        None => Ok(txs),
    }
}

fn prepare_order(
    msg: &MergeableBlockV1,
    order_ref: &MergeOrderRef,
    txs: &Arc<Vec<Arc<DecodedTx>>>,
    block_hash: B256,
) -> PreparedOrder {
    let (order_txs, reverting, dropping): (Vec<Arc<DecodedTx>>, Vec<usize>, Vec<usize>) =
        match order_ref {
            MergeOrderRef::Tx(tx) => (
                vec![txs[tx.index as usize].clone()],
                if tx.can_revert { vec![0] } else { vec![] },
                vec![],
            ),
            MergeOrderRef::Bundle(bundle) => (
                bundle.txs.iter().map(|&i| txs[i as usize].clone()).collect(),
                bundle.reverting_txs.iter().map(|&i| i as usize).collect(),
                bundle.dropping_txs.iter().map(|&i| i as usize).collect(),
            ),
        };

    // Canonical order hash: single tx = keccak(tx bytes) (== tx hash);
    // bundle = keccak(concat tx hashes).
    let order_hash = match order_ref {
        MergeOrderRef::Tx(_) => order_txs[0].hash,
        MergeOrderRef::Bundle(_) => {
            let hashes: Vec<B256> = order_txs.iter().map(|tx| tx.hash).collect();
            bundle_order_hash(&hashes)
        }
    };
    let meta = OrderMeta {
        order_hash,
        builder_pubkey: msg.builder_pubkey,
        origin_coinbase: msg.builder_address,
        source_block_hash: block_hash,
    };

    PreparedOrder {
        order_id: meta.order_id(),
        order_hash,
        latest_only: matches!(order_ref, MergeOrderRef::Bundle(b) if b.latest_only),
        origin: msg.builder_address,
        builder_pubkey: msg.builder_pubkey,
        source_block_hash: block_hash,
        source_block_value: msg.block_value,
        txs: order_txs,
        reverting,
        dropping,
    }
}

/// Test/scaffolding engine: pools nothing and rejects every activation.
#[cfg(test)]
pub struct NoopEngine;

#[cfg(test)]

impl NoopEngine {
    pub fn spawn(
        events: crossbeam_channel::Receiver<EngineEvent>,
        out: crossbeam_channel::Sender<EngineOutput>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("noop-merge-engine".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if let EngineEvent::ActivateBase { slot, block_hash, generation, .. } = event {
                        let _ = out.send(EngineOutput::reject(
                            generation,
                            slot,
                            RejectCode::UnknownBaseBlock,
                            RejectSubject::BlockHash(block_hash),
                            "noop engine",
                        ));
                    }
                }
            })
            .expect("failed to spawn noop engine thread")
    }
}
