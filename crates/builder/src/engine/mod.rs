//! Merge engine: consumes decoded protocol events from the TCP server tile
//! over a bounded channel, maintains the per-slot order pool and merge
//! session, and streams merged blocks / rejects back. Everything
//! ethrex-related stays behind this boundary.

pub mod convert;
pub mod error;
pub mod payment;
pub mod session;
pub mod simulate;
#[cfg(test)]
mod tests;
pub mod types;

use std::{sync::Arc, time::Duration};

use alloy_primitives::{B256, keccak256};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use ethrex_blockchain::Blockchain;
use ethrex_crypto::native::NativeCrypto;
use ethrex_storage::Store;
use helix_tcp_types::merging::{
    builder_to_relay::{MergedBlockV1, RejectCode, RejectSubject, RejectV1},
    control::RelayConfigV1,
    order::{
        MAX_BLOCK_TXS, MAX_ORDERS_PER_BLOCK, MAX_TX_BYTES, MergeOrderRef, OrderMeta,
        bundle_order_hash,
    },
    relay_to_builder::{MergeableBlockV1, SlotStartV1},
};
use ssz::Decode;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::{
    engine::{
        error::MergeError,
        session::{EmitOutcome, MergeSession},
        types::{
            DecodedTx, EngineConfig, MAX_PARKED_SESSIONS, PreparedBlock, PreparedOrder, SlotState,
        },
    },
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
        recv_ns: u64,
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
    config: EngineConfig,
    store: Store,
    blockchain: Arc<Blockchain>,
    head: watch::Receiver<HeadInfo>,
    out: Sender<EngineOutput>,
    /// Latest connection generation; events from older connections are dropped.
    generation: u64,
    /// Latest relay config; snapshotted into the slot at `SlotStart`.
    relay_config: Option<RelayConfigV1>,
    slot: Option<SlotState>,
}

impl MergeEngine {
    pub fn spawn(
        config: EngineConfig,
        store: Store,
        blockchain: Arc<Blockchain>,
        head: watch::Receiver<HeadInfo>,
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
                let mut engine = MergeEngine {
                    config,
                    store,
                    blockchain,
                    head,
                    out,
                    generation: 0,
                    relay_config: None,
                    slot: None,
                };
                info!("merge engine started");
                engine.run(events);
                info!("merge engine stopped");
            })
            .expect("failed to spawn merge engine thread")
    }

    fn run(&mut self, events: Receiver<EngineEvent>) {
        loop {
            // A throttled improvement is waiting: wake up after the spacing
            // window even if no further events arrive (slot-tail emissions
            // would otherwise be lost).
            let first = if self.has_pending_emission() {
                let wait = self.config.min_emission_interval.max(Duration::from_millis(1));
                match events.recv_timeout(wait) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match events.recv() {
                    Ok(event) => Some(event),
                    Err(_) => break,
                }
            };

            // A timeout wake-up is itself work: retry the pending emission.
            let mut work = match first {
                Some(event) => self.handle_event(event),
                None => true,
            };
            // Coalesce everything already queued into one merge pass.
            while let Ok(event) = events.try_recv() {
                work |= self.handle_event(event);
            }
            if work {
                self.merge_pass();
            }
        }
    }

    fn has_pending_emission(&self) -> bool {
        self.slot
            .as_ref()
            .and_then(|state| state.session.as_ref())
            .is_some_and(|session| session.pending_emission)
    }

    /// Logs the stats of every session (live + parked) and drops the slot.
    fn teardown_slot(&mut self, reason: &str) {
        if let Some(state) = self.slot.take() {
            if let Some(session) = &state.session {
                session.log_stats(reason);
            }
            for session in &state.parked {
                session.log_stats(reason);
            }
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
                self.relay_config = Some(config);
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
                state.relay_config = self.relay_config.clone();
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
            EngineEvent::ActivateBase { slot, block_hash, recv_ns, generation } => {
                if generation != self.generation {
                    return false;
                }
                let current_slot = self.slot.as_ref().map(|s| s.slot);
                if current_slot != Some(slot) {
                    let _ = self.out.send(EngineOutput::reject(
                        self.generation,
                        slot,
                        RejectCode::StaleSlot,
                        RejectSubject::BlockHash(block_hash),
                        "activation for a slot that is not current",
                    ));
                    return false;
                }
                let state = self.slot.as_mut().expect("checked above");
                if state.session.as_ref().is_some_and(|s| s.base_block_hash == block_hash) {
                    return false;
                }
                state.pending_activation = Some((block_hash, recv_ns));
                true
            }
        }
    }

    /// One merge pass: resolve a pending activation, extend the session with
    /// the pooled orders, emit if improved.
    fn merge_pass(&mut self) {
        let Some(state) = self.slot.as_mut() else { return };
        let Some(relay_config) = state.relay_config.clone() else {
            // No config yet: nothing can be merged (no collateral/distribution).
            return;
        };

        // Resolve a pending activation once its block is pooled.
        if let Some((block_hash, activate_recv_ns)) = state.pending_activation {
            if let Some(base) = state.blocks.get(&block_hash) {
                state.pending_activation = None;
                if !base.allow_appending {
                    let _ = self.out.send(EngineOutput::reject(
                        self.generation,
                        state.slot,
                        RejectCode::UnknownBaseBlock,
                        RejectSubject::BlockHash(block_hash),
                        "base block was not forwarded as appendable",
                    ));
                } else {
                    // Park the outgoing session: the relay's top bid often
                    // flips back, and resuming skips the base re-replay.
                    if let Some(old) = state.session.take() {
                        debug!(base_block_hash = %old.base_block_hash, "parking session");
                        state.parked.push(old);
                        if state.parked.len() > MAX_PARKED_SESSIONS {
                            let evicted = state.parked.remove(0);
                            evicted.log_stats("parked_eviction");
                        }
                    }

                    let head = *self.head.borrow();
                    let parked_ix =
                        state.parked.iter().position(|s| s.base_block_hash == block_hash);
                    if let Some(ix) = parked_ix {
                        // Resume: the base was already validated and replayed
                        // when the session was first built and the slot's
                        // parent is fixed; only re-check sync.
                        if head.is_synced {
                            info!(
                                slot = state.slot,
                                base_block_hash = %block_hash,
                                "merge session resumed from parked"
                            );
                            state.session = Some(state.parked.remove(ix));
                        } else {
                            let session = state.parked.remove(ix);
                            session.log_stats("resume_not_synced");
                            let _ = self.out.send(EngineOutput::reject(
                                self.generation,
                                state.slot,
                                RejectCode::NotSynced,
                                RejectSubject::BlockHash(block_hash),
                                "builder lost sync while session was parked",
                            ));
                        }
                    } else {
                        // Head gating: the base must build on our synced head.
                        let head_hash = convert::b256(head.hash);
                        let result = if !head.is_synced {
                            Err(MergeError::NotSynced)
                        } else if head_hash != state.parent_hash {
                            Err(MergeError::HeadMismatch)
                        } else {
                            MergeSession::activate(
                                state,
                                base,
                                &self.store,
                                self.blockchain.clone(),
                                &relay_config,
                            )
                        };
                        match result {
                            Ok(session) => {
                                info!(
                                    slot = state.slot,
                                    base_block_hash = %block_hash,
                                    activate_to_replay_us = crate::utils::utcnow_ns()
                                        .saturating_sub(activate_recv_ns) /
                                        1000,
                                    "merge session activated"
                                );
                                state.session = Some(session);
                            }
                            Err(err) => {
                                warn!(%err, base_block_hash = %block_hash, "activation failed");
                                if let Some((code, subject)) = err.reject(Some(block_hash)) {
                                    let _ = self.out.send(EngineOutput::reject(
                                        self.generation,
                                        state.slot,
                                        code,
                                        subject,
                                        err.to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        let SlotState { slot, proposer_fee_recipient, orders, session, .. } = state;
        let Some(session) = session.as_mut() else { return };

        let changed = session.try_extend(orders);
        if !changed && !session.pending_emission && !session.has_pending_revenue() {
            return;
        }
        match session.emit(*slot, *proposer_fee_recipient, &relay_config, &self.config) {
            Ok(EmitOutcome::Emitted(msg)) => {
                let _ = self.out.send(EngineOutput::Merged { generation: self.generation, msg });
            }
            // Throttled sets `pending_emission`; the worker loop retries after
            // the spacing window.
            Ok(EmitOutcome::Throttled) | Ok(EmitOutcome::NotImproved) => {}
            Err(err) => warn!(%err, "emission failed"),
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
        if state.orders_admitted + msg.merge_orders.len() > self.config.max_orders_per_slot {
            return Err(fail(MergeError::LimitExceeded("max orders per slot".into())));
        }
        for order in &msg.merge_orders {
            order
                .validate(tx_bytes.len())
                .map_err(|_| fail(MergeError::InvalidOrder("order ref out of range".into())))?;
        }

        // Decode txs and recover senders in parallel, through the per-slot
        // cache (incremental submissions share most txs).
        let decoded = decode_block_txs(&msg, &mut state.recovery_cache)
            .map_err(|err| fail(MergeError::InvalidOrder(err)))?;
        let txs = Arc::new(decoded);

        // Extract the order pool entries.
        for order_ref in &msg.merge_orders {
            let prepared = prepare_order(&msg, order_ref, &txs, block_hash);
            state.orders_admitted += 1;
            match state.order_ids.get(&prepared.order_id) {
                Some(&existing_ix) => {
                    // Duplicate order: attribution goes to the highest-value
                    // source block.
                    let existing = &state.orders[existing_ix];
                    if msg.block_value > existing.source_block_value {
                        state.orders[existing_ix] = prepared;
                    }
                }
                None => {
                    state.order_ids.insert(prepared.order_id, state.orders.len());
                    state.orders.push(prepared);
                }
            }
        }

        debug!(
            slot = msg.slot,
            %block_hash,
            txs = txs.len(),
            orders = msg.merge_orders.len(),
            pool = state.orders.len(),
            allow_appending = msg.allow_appending,
            "mergeable block pooled"
        );

        state.blocks.insert(block_hash, PreparedBlock {
            block_hash,
            builder_pubkey: msg.builder_pubkey,
            block_value: msg.block_value,
            allow_appending: msg.allow_appending,
            payload: msg.execution_payload,
            txs,
            recv_ns,
        });
        Ok(())
    }
}

/// Decodes every tx in the payload and recovers senders (cache-assisted, the
/// misses in parallel).
fn decode_block_txs(
    msg: &MergeableBlockV1,
    recovery_cache: &mut rustc_hash::FxHashMap<B256, ethrex_common::Address>,
) -> Result<Vec<Arc<DecodedTx>>, String> {
    use rayon::prelude::*;

    let tx_bytes = &msg.execution_payload.payload_inner.payload_inner.transactions;

    struct Partial {
        tx: ethrex_common::types::Transaction,
        hash: B256,
        cached_sender: Option<ethrex_common::Address>,
    }

    let partials: Vec<Partial> = tx_bytes
        .iter()
        .map(|bytes| {
            let tx = ethrex_common::types::Transaction::decode_canonical(bytes)
                .map_err(|e| format!("tx decode: {e}"))?;
            let hash = keccak256(bytes);
            Ok(Partial { tx, hash, cached_sender: recovery_cache.get(&hash).copied() })
        })
        .collect::<Result<_, String>>()?;

    let decoded: Vec<Arc<DecodedTx>> = partials
        .into_par_iter()
        .map(|partial| {
            let sender = match partial.cached_sender {
                Some(sender) => sender,
                None => {
                    partial.tx.sender(&NativeCrypto).map_err(|e| format!("sender recovery: {e}"))?
                }
            };
            let blob_hashes =
                partial.tx.blob_versioned_hashes().into_iter().map(convert::b256).collect();
            Ok(Arc::new(DecodedTx {
                gas_limit: partial.tx.gas_limit(),
                blob_hashes,
                hash: partial.hash,
                sender,
                tx: partial.tx,
            }))
        })
        .collect::<Result<_, String>>()?;

    for tx in &decoded {
        recovery_cache.insert(tx.hash, tx.sender);
    }
    Ok(decoded)
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
