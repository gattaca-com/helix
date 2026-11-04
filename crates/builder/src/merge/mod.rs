mod block;
mod effects;
mod keccak;
mod lists;
mod payment;
mod pool;
pub mod record;
mod rlp;
mod root;
mod state;
#[cfg(test)]
mod tests;
mod trie;

use std::{
    cell::RefCell,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use dashmap::DashSet;
use ethrex_storage::Store;
use flux::{
    spine::{DCacheRead, SpineAdapter, SpineDCacheConsumer, SpineProducers},
    tile::Tile,
};
use helix_tcp_types::merging::{
    builder_to_relay::{RejectCode, RejectSubject, RejectV1},
    control::RelayConfigV1,
    relay_to_builder::SlotStartV1,
};
use rustc_hash::FxHashSet;
use ssz::{Decode, Encode};
use tracing::{debug, info, warn};

use crate::{
    convert::{aaddr, eaddr},
    merge::{
        block::{BlockMerger, MergeScratch, ParentState, SlotCtx},
        pool::{BaseBlock, Pool, Reject, Submitter},
        record::{RecordConfig, SlotRecorder},
        state::KeyTable,
        trie::NodeArena,
    },
    metrics,
    spine::{
        BuilderSpine, BuilderSpineProducers, EngineCursor, EventKind, FromEngine, OutputKind,
        ToEngine,
    },
    utils::utcnow_ns,
};

pub const RELAY_KEY_ENV: &str = "RELAY_KEY";

pub const STACK_SIZE: usize = 32 << 20;

pub struct EngineConfig {
    pub relay_signer: PrivateKeySigner,
    pub max_blocks_per_slot: usize,
    pub max_orders_per_slot: usize,
    pub max_builders: usize,
    pub min_value_increase_wei: U256,
    pub max_base_age: Duration,
    // Run each builder's first base once, unsent, to pay its cold costs.
    pub warmup: bool,
    pub blacklist: Arc<DashSet<Address>>,
    pub record: Option<RecordConfig>,
}

impl EngineConfig {
    pub fn load_relay_signer() -> PrivateKeySigner {
        let key = std::env::var(RELAY_KEY_ENV)
            .unwrap_or_else(|_| panic!("{RELAY_KEY_ENV} env var not set"));
        key.parse().expect("failed to parse RELAY_KEY")
    }
}

struct Builder {
    coinbase: Address,
    bases: Vec<BaseBlock>,
    merger: BlockMerger,
}

impl Builder {
    fn upsert_base(&mut self, base: BaseBlock, submitters: &[Submitter]) {
        let pubkey = &submitters[base.submitter as usize].pubkey;
        match self.bases.iter().position(|b| submitters[b.submitter as usize].pubkey == *pubkey) {
            Some(i) => self.bases[i] = base,
            None => self.bases.push(base),
        }
    }

    fn top_base(&self, values: &[U256]) -> Option<usize> {
        let value = |i: usize| values[self.bases[i].block as usize];
        (0..self.bases.len()).reduce(|best, i| if value(i) > value(best) { i } else { best })
    }
}

fn select_base(
    builders: &[Builder],
    pool: &Pool,
    now_ns: u64,
    max_age_ns: u64,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, U256)> = None;
    for (i, builder) in builders.iter().enumerate() {
        let Some(top) = builder.top_base(&pool.block_values) else { continue };
        let base = &builder.bases[top];
        let m = &builder.merger;
        let has_work = m.base.hash != base.hash ||
            (m.live &&
                m.seen_version != pool.orders_version &&
                now_ns.saturating_sub(m.base.recv_ns) < max_age_ns);
        let value = pool.block_values[base.block as usize];
        if has_work && best.is_none_or(|(.., v)| value > v) {
            best = Some((i, top, value));
        }
    }
    best.map(|(i, top, _)| (i, top))
}

pub struct MergeEngine {
    config: EngineConfig,
    store: Store,
    cursor: Arc<EngineCursor>,
    // Copied out of the dcache: a lapped read is only detected after the copy.
    event_buf: Vec<u8>,
    ssz_buf: Vec<u8>,
    generation: u64,
    // `next_relay` is promoted to `relay` at slot start; `relay` is Some while `slot` is.
    relay: Option<RelayConfigV1>,
    next_relay: Option<RelayConfigV1>,
    // None between slots; `pool`, `builders`, `parent` and `recorder` are empty then.
    slot: Option<SlotStartV1>,
    pool: Pool,
    builders: Vec<Builder>,
    // Reset builders from ended slots; reused so their merge buffers stay allocated.
    spare: Vec<Builder>,
    parent: Option<ParentState>,
    // Valid while `parent` is Some; kept across slots to reuse its tables.
    keys: Option<KeyTable>,
    // Snapshot of `config.blacklist` taken at slot start: the shared set is refreshed off-thread.
    blacklist: FxHashSet<Address>,
    recorder: Option<Arc<SlotRecorder>>,
    scratch: MergeScratch,
    parent_nodes: NodeArena,
}

impl MergeEngine {
    pub fn new(config: EngineConfig, store: Store, cursor: Arc<EngineCursor>) -> Self {
        let builders = Vec::with_capacity(config.max_builders);
        let spare = Vec::with_capacity(config.max_builders);
        MergeEngine {
            config,
            store,
            cursor,
            event_buf: Vec::new(),
            ssz_buf: Vec::new(),
            generation: 0,
            relay: None,
            next_relay: None,
            slot: None,
            pool: Pool::default(),
            builders,
            spare,
            parent: None,
            keys: None,
            blacklist: FxHashSet::default(),
            recorder: None,
            scratch: MergeScratch::default(),
            // ~31k parent nodes loaded per slot measured; `trim` keeps several slots' worth.
            parent_nodes: NodeArena::new(false, 1 << 18, 1 << 21),
        }
    }

    fn apply_event(&mut self, event: ToEngine, body: &[u8], out: &BuilderSpineProducers) {
        if let Some(recorder) = &self.recorder {
            let slot_end = event.slot.to_le_bytes();
            let body = if event.kind == EventKind::SlotEnd { &slot_end[..] } else { body };
            recorder.record_event(utcnow_ns(), event.kind, body);
        }
        // A lost ConnectionReset must not leave the live connection's blocks dropped.
        if event.generation > self.generation {
            self.generation = event.generation;
            self.end_slot("connection_reset");
        }
        match event.kind {
            EventKind::ConnectionReset => {}
            EventKind::RelayConfig => {
                let Ok(config) = RelayConfigV1::from_ssz_bytes(body) else {
                    warn!("undecodable relay config");
                    return;
                };
                info!(
                    relay_bps = config.relay_bps,
                    merged_builder_bps = config.merged_builder_bps,
                    winning_builder_bps = config.winning_builder_bps,
                    collaterals = config.builder_collaterals.len(),
                    "relay config received"
                );
                self.next_relay = Some(config);
            }
            EventKind::SlotStart => match SlotStartV1::from_ssz_bytes(body) {
                Ok(msg) => self.start_slot(msg, body),
                Err(err) => warn!(?err, "undecodable slot start"),
            },
            EventKind::SlotEnd => {
                if self.slot.is_some_and(|s| s.slot == event.slot) {
                    self.end_slot("slot_end");
                }
            }
            EventKind::MergeableBlock => {
                if event.generation == self.generation {
                    self.on_block(body, event.recv_ns, out);
                }
            }
        }
    }

    #[cold]
    fn lost_event(&mut self, kind: Option<EventKind>, out: &BuilderSpineProducers) {
        // Busy makes the relay resend txs whole, so later references still resolve.
        metrics::queue_drop("engine_lapped");
        warn!(?kind, "engine event lost");
        let slot = self.slot.map_or(0, |s| s.slot);
        let subject = RejectSubject::None(0);
        let msg = b"engine event lost".to_vec();
        let msg = RejectV1 { slot, code: RejectCode::Busy, subject, msg };
        publish(out, OutputKind::Reject, self.generation, &mut self.ssz_buf, &msg);
    }

    fn start_slot(&mut self, msg: SlotStartV1, body: &[u8]) {
        if let Some(slot) = &self.slot &&
            (msg.slot < slot.slot ||
                (msg.slot == slot.slot && msg.parent_hash == slot.parent_hash))
        {
            return;
        }
        self.end_slot("slot_start");
        if let Some(relay) = self.next_relay.take() {
            self.relay = Some(relay);
        }
        let Some(relay) = &self.relay else { return };
        self.recorder = self
            .config
            .record
            .as_ref()
            .filter(|record| msg.slot.is_multiple_of(record.every_n_slots))
            .map(|record| {
                let signer = eaddr(self.config.relay_signer.address());
                let safes = relay.builder_collaterals.iter().map(|c| eaddr(c.collateral_safe));
                let recorder = SlotRecorder::new(record, msg.slot, signer, safes.collect());
                let now = utcnow_ns();
                recorder.record_event(now, EventKind::RelayConfig, &relay.as_ssz_bytes());
                recorder.record_event(now, EventKind::SlotStart, body);
                recorder
            });
        self.blacklist.clear();
        if msg.apply_blacklist {
            self.blacklist.extend(self.config.blacklist.iter().map(|a| *a));
        }
        debug!(slot = msg.slot, parent_hash = %msg.parent_hash, "slot start");
        self.slot = Some(msg);
    }

    fn end_slot(&mut self, reason: &str) {
        let Some(slot) = self.slot.take() else { return };
        if let Some(recorder) = self.recorder.take() {
            recorder.finish();
        }
        let (blocks, orders) = (self.pool.block_hashes.len(), self.pool.orders.len());
        let builders = self.builders.len();
        info!(reason, slot = slot.slot, blocks, orders, builders, "merge slot ended");
        metrics::slot_pool(blocks, orders, builders);
        self.pool.clear();
        for mut builder in self.builders.drain(..) {
            builder.bases.clear();
            builder.merger.reset();
            self.spare.push(builder);
        }
        self.parent = None;
        self.parent_nodes.trim();
    }

    fn on_block(&mut self, body: &[u8], recv_ns: u64, out: &BuilderSpineProducers) {
        let start = std::time::Instant::now();
        let Some(slot) = self.slot else {
            let subject = RejectSubject::None(0);
            let msg = b"stale slot".to_vec();
            let msg = RejectV1 { slot: 0, code: RejectCode::StaleSlot, subject, msg };
            publish(out, OutputKind::Reject, self.generation, &mut self.ssz_buf, &msg);
            metrics::rejection("merge_submission", "stale_slot");
            return;
        };
        let added = self.pool.add(
            body,
            recv_ns,
            slot.slot,
            slot.parent_hash,
            self.config.max_blocks_per_slot,
            self.config.max_orders_per_slot,
        );
        metrics::stage_latency("merge_submission", start.elapsed().as_micros() as u64);
        let mut send = |reject: Reject| {
            metrics::rejection("merge_submission", reject.label);
            warn!(label = reject.label, msg = reject.msg, "mergeable block rejected");
            let subject =
                reject.block_hash.map_or(RejectSubject::None(0), RejectSubject::BlockHash);
            let msg = RejectV1 {
                slot: slot.slot,
                code: reject.code,
                subject,
                msg: reject.msg.into_bytes(),
            };
            publish(out, OutputKind::Reject, self.generation, &mut self.ssz_buf, &msg);
        };
        let added = match added {
            Ok(added) => added,
            Err(reject) => return send(reject),
        };
        if let Some(reject) = added.orders_dropped {
            send(reject);
        }
        let Some(base) = added.base else { return };
        let coinbase = aaddr(base.header.coinbase);
        let i = match self.builders.iter().position(|b| b.coinbase == coinbase) {
            Some(i) => i,
            None if self.builders.len() < self.config.max_builders => {
                let mut builder = self.spare.pop().unwrap_or_else(|| Builder {
                    coinbase,
                    bases: Vec::new(),
                    merger: BlockMerger::new(),
                });
                builder.coinbase = coinbase;
                self.builders.push(builder);
                self.builders.len() - 1
            }
            None => return metrics::rejection("merge", "max_builders"),
        };
        self.builders[i].upsert_base(base, &self.pool.submitters);
    }

    fn do_work(&mut self, out: &BuilderSpineProducers) -> bool {
        let (Some(slot), Some(relay)) = (self.slot, &self.relay) else { return false };
        let now = utcnow_ns();
        let max_age_ns = self.config.max_base_age.as_nanos() as u64;
        let Some((i, top)) = select_base(&self.builders, &self.pool, now, max_age_ns) else {
            return false;
        };
        if self.parent.is_none() {
            match ParentState::open(&self.store, slot.parent_hash, self.recorder.as_ref()) {
                Ok(parent) => {
                    match &mut self.keys {
                        Some(keys) => keys.reset(parent.db.clone()),
                        None => self.keys = Some(KeyTable::new(parent.db.clone())),
                    }
                    self.parent = Some(parent);
                }
                Err(err) => {
                    debug!(%err, "parent state not ready");
                    metrics::rejection("merge", "parent_not_ready");
                    self.builders[i].bases.swap_remove(top);
                    return true;
                }
            }
        }
        let (Some(parent), Some(keys)) = (&self.parent, &mut self.keys) else { return true };
        let cx = SlotCtx {
            store: &self.store,
            parent,
            relay,
            slot: slot.slot,
            proposer: slot.proposer_fee_recipient,
            beacon_root: slot.parent_beacon_block_root,
            blacklist: slot.apply_blacklist.then_some(&self.blacklist),
            signer: &self.config.relay_signer,
            min_increase: self.config.min_value_increase_wei,
        };
        let builder = &mut self.builders[i];
        // A real block hash is never zero: zero means no base has been opened yet.
        let warming = self.config.warmup && builder.merger.base.hash == B256::ZERO;
        let start = std::time::Instant::now();
        let base = &builder.bases[top];
        let merger = &mut builder.merger;
        if merger.base.hash != base.hash {
            if let Err(reason) = merger.open_base(base, &self.pool, &cx, keys, &mut self.scratch) {
                metrics::rejection("merge_open", reason);
                debug!(base = %base.hash, reason, "base not merged");
                builder.bases.swap_remove(top);
                return true;
            }
            if !warming {
                metrics::stage_latency("merge_open", start.elapsed().as_micros() as u64);
            }
        }
        if !merger.live {
            return true;
        }
        let extend_start = std::time::Instant::now();
        merger.extend(&self.pool, &cx, keys, &mut self.scratch);
        let emit_start = std::time::Instant::now();
        let emitted = merger.emit(&self.pool, &cx, keys, &mut self.parent_nodes, &mut self.scratch);
        if warming {
            debug!(base = %merger.base.hash, builder = %builder.coinbase, "base warmed");
            merger.live = false;
            return true;
        }
        metrics::stage_latency("merge_extend", (emit_start - extend_start).as_micros() as u64);
        match emitted {
            Ok(Some(msg)) => {
                let emitted = utcnow_ns();
                if merger.emissions == 1 {
                    let age_us = emitted.saturating_sub(merger.base.recv_ns) / 1000;
                    metrics::stage_latency("merge_first_emit", age_us);
                }
                metrics::stage_latency("merge_emit", emit_start.elapsed().as_micros() as u64);
                info!(
                    base = %merger.base.hash,
                    builder = %builder.coinbase,
                    emission = merger.emissions,
                    base_age_us = emitted.saturating_sub(merger.base.recv_ns) / 1000,
                    step_us = start.elapsed().as_micros() as u64,
                    orders = msg.included_order_ids.len(),
                    proposer_value = %msg.proposer_value,
                    "merged block emitted"
                );
                publish(out, OutputKind::Merged, self.generation, &mut self.ssz_buf, &msg);
            }
            Ok(None) => {}
            Err(reason) => {
                metrics::rejection("merge_emit", reason);
                debug!(base = %merger.base.hash, reason, "base dropped");
                merger.live = false;
            }
        }
        true
    }
}

impl Tile<BuilderSpine> for MergeEngine {
    fn on_attach(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        // Otherwise the consumer starts at the producer's head and misses earlier events.
        let events: &mut SpineDCacheConsumer<ToEngine> = adapter.consumers.as_mut();
        events.inner.subscribe_broadcast();
    }

    fn loop_body(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        let body = RefCell::new(std::mem::take(&mut self.event_buf));
        let mut read_to = None;
        adapter.consume_with_dcache(
            |_, payload: &[u8]| {
                let mut body = body.borrow_mut();
                body.clear();
                body.extend_from_slice(payload);
            },
            |read, out| match read {
                DCacheRead::Ok((event, ())) => {
                    read_to = Some(event);
                    self.apply_event(event, &body.borrow(), out);
                }
                DCacheRead::NoRef(event) => {
                    read_to = Some(event);
                    self.apply_event(event, &[], out);
                }
                DCacheRead::Lost(event) => {
                    read_to = Some(event);
                    self.lost_event(Some(event.kind), out);
                }
                DCacheRead::SpedPast => self.lost_event(None, out),
            },
        );
        self.event_buf = body.into_inner();
        if let Some(event) = read_to {
            self.cursor.charge.store(event.charge, Ordering::Release);
            self.cursor.seq.store(event.seq, Ordering::Release);
        }
        if self.do_work(&adapter.producers) {
            adapter.mark_work();
        }
    }

    fn teardown(mut self, _adapter: &mut SpineAdapter<BuilderSpine>) {
        self.end_slot("shutdown");
    }
}

fn publish(
    out: &BuilderSpineProducers,
    kind: OutputKind,
    generation: u64,
    encode: &mut Vec<u8>,
    msg: &impl Encode,
) {
    encode.clear();
    msg.ssz_append(encode);
    let header = FromEngine { kind, _pad: [0; 7], generation };
    let write = |buf: &mut [u8]| buf.copy_from_slice(encode);
    if let Err(err) = out.produce_with_dcache(header, Some((encode.len(), write))) {
        metrics::queue_drop("engine_output");
        warn!(%err, ?kind, len = encode.len(), "engine output dropped");
    }
}
