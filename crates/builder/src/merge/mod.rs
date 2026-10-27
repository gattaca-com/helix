//! Merge engine. One thread owns everything: it decodes forwarded blocks into the slot's order
//! pool, keeps one merge per builder on that builder's newest appendable base, extends a merge
//! whenever the pool moves, and emits every merged block that beats the last one on its base.
//!
//! Each loop drains every queued event, then does one unit of merge work: open a builder's newer
//! base (run it, extend it, emit), or extend and emit one live merge. Ingest never waits behind
//! more than one unit. Newer bases go first: a base its builder has superseded cannot win.

mod block;
mod effects;
mod ingest;
mod keccak;
mod lists;
mod payment;
pub mod record;
mod rlp;
mod root;
mod state;
#[cfg(test)]
mod tests;
mod trie;

use std::{cell::RefCell, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use dashmap::DashSet;
use ethrex_storage::Store;
use helix_tcp_types::merging::{
    builder_to_relay::{MergedBlockV1, RejectCode, RejectSubject, RejectV1},
    control::RelayConfigV1,
    relay_to_builder::SlotStartV1,
};
use tracing::{debug, info, warn};

use crate::{
    convert::{aaddr, eaddr},
    merge::{
        block::{Cx, Merge, Parent, Scratch},
        ingest::{Base, Pool},
        record::{RecordConfig, SlotRecorder},
        trie::Arena,
    },
    metrics,
    utils::utcnow_ns,
};

/// Environment variable holding the relay-owned Safe signer key.
pub const RELAY_KEY_ENV: &str = "RELAY_KEY";

/// The EVM recurses per call frame.
const STACK: usize = 32 << 20;

/// Events from the server tile. `generation` identifies the relay connection: it bumps whenever
/// the active connection is replaced, and outputs carrying a stale one are dropped by the tile.
pub enum EngineEvent {
    /// The relay connection was replaced or lost; expect a full slot replay.
    ConnectionReset {
        generation: u64,
    },
    /// Distribution policy; takes effect at the next `SlotStart`.
    RelayConfig(RelayConfigV1),
    SlotStart(SlotStartV1),
    SlotEnd {
        slot: u64,
    },
    MergeableBlock {
        body: Vec<u8>,
        recv_ns: u64,
        generation: u64,
    },
}

/// Outputs to the server tile, which stamps `response_id` right before sending.
pub enum EngineOutput {
    Merged { generation: u64, msg: Box<MergedBlockV1> },
    Reject { generation: u64, msg: RejectV1 },
}

impl EngineOutput {
    pub fn generation(&self) -> u64 {
        match self {
            EngineOutput::Merged { generation, .. } | EngineOutput::Reject { generation, .. } => {
                *generation
            }
        }
    }
}

pub struct EngineConfig {
    /// Safe owner key that signs the distribution tx.
    pub relay_signer: PrivateKeySigner,
    pub max_blocks_per_slot: usize,
    pub max_orders_per_slot: usize,
    /// Builders merged at once; one more is ignored for the slot. 0 disables merging.
    pub max_builders: usize,
    /// A merged block must beat the base's last emission by more than this.
    pub min_value_increase_wei: U256,
    /// Stop improving a base this long after it arrived.
    pub max_base_age: Duration,
    /// Merge each builder's first base of the slot through one emission, then drop it unsent with
    /// its latencies unrecorded: it pays the builder's cold costs, a state root written in full.
    pub warmup: bool,
    pub core: Option<usize>,
    /// Relay blacklist, refreshed by the simulation role.
    pub disallow: Arc<DashSet<Address>>,
    pub record: Option<RecordConfig>,
}

impl EngineConfig {
    pub fn load_relay_signer() -> PrivateKeySigner {
        let key = std::env::var(RELAY_KEY_ENV)
            .unwrap_or_else(|_| panic!("{RELAY_KEY_ENV} env var not set"));
        key.parse().expect("failed to parse RELAY_KEY")
    }
}

/// One builder's merge work: its newest base not yet opened, and the merge on the base before.
struct Builder {
    coinbase: Address,
    pending: Option<Base>,
    /// Made at the builder's first open and reused for each newer base.
    merge: Option<Merge>,
}

struct Slot {
    slot: u64,
    parent_hash: B256,
    proposer: Address,
    beacon_root: B256,
    blacklist: bool,
    /// Snapshot at `SlotStart`.
    relay: RelayConfigV1,
    pool: Pool,
    /// Opened at the first base; `None` until the node has the parent.
    parent: Option<Parent>,
    builders: Vec<Builder>,
    /// Round-robin cursor over `builders`.
    next: usize,
    recorder: Option<Arc<SlotRecorder>>,
}

impl Slot {
    /// The builder to serve next: one with a newer base first, else one whose merge can still
    /// gain from the pool.
    fn pick(&self, now_ns: u64, max_age_ns: u64) -> Option<usize> {
        let n = self.builders.len();
        let order = (0..n).map(|i| (self.next + i) % n);
        let improvable = |b: &Builder| {
            b.merge.as_ref().is_some_and(|m| {
                m.live &&
                    m.seen != self.pool.version &&
                    now_ns.saturating_sub(m.base.recv_ns) < max_age_ns
            })
        };
        order
            .clone()
            .find(|&i| self.builders[i].pending.is_some())
            .or_else(|| order.into_iter().find(|&i| improvable(&self.builders[i])))
    }
}

pub struct MergeEngine {
    config: EngineConfig,
    store: Store,
    out: Sender<EngineOutput>,
    /// Latest connection; blocks from older ones are dropped.
    generation: u64,
    relay: Option<RelayConfigV1>,
    slot: Option<Slot>,
    /// The last slot's pool and builders, cleared; the next slot reuses their capacity.
    spare: (Pool, Vec<Builder>),
    scratch: Scratch,
    nodes: RefCell<Arena>,
}

impl MergeEngine {
    pub fn spawn(
        config: EngineConfig,
        store: Store,
        events: Receiver<EngineEvent>,
        out: Sender<EngineOutput>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("merge-engine".into())
            .stack_size(STACK)
            .spawn(move || {
                if let Some(core) = config.core &&
                    !core_affinity::set_for_current(core_affinity::CoreId { id: core })
                {
                    warn!(core, "failed to pin merge engine thread");
                }
                let spare = (Pool::default(), Vec::with_capacity(config.max_builders));
                let mut engine = MergeEngine {
                    config,
                    store,
                    out,
                    generation: 0,
                    relay: None,
                    slot: None,
                    spare,
                    scratch: Scratch::default(),
                    // Up to ~31k nodes loaded per slot measured, each branch ~15 hashed children;
                    // `trim` keeps several slots' worth.
                    nodes: RefCell::new(Arena::new(false, 1 << 18, 1 << 21)),
                };
                info!("merge engine started");
                engine.run(&events);
                info!("merge engine stopped");
            })
            .expect("failed to spawn merge engine thread")
    }

    fn run(&mut self, events: &Receiver<EngineEvent>) {
        let mut busy = false;
        loop {
            // With nothing to improve, only an event can create work.
            let event = match busy {
                true => match events.try_recv() {
                    Ok(event) => Some(event),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                },
                false => match events.recv() {
                    Ok(event) => Some(event),
                    Err(_) => return,
                },
            };
            if let Some(event) = event {
                self.apply(event);
                while let Ok(event) = events.try_recv() {
                    self.apply(event);
                }
            }
            busy = self.step();
        }
    }

    fn apply(&mut self, event: EngineEvent) {
        if let Some(recorder) = self.slot.as_ref().and_then(|s| s.recorder.as_ref()) {
            recorder.event(utcnow_ns(), &event);
        }
        match event {
            EngineEvent::ConnectionReset { generation } => {
                self.generation = generation;
                self.end_slot("connection_reset");
            }
            EngineEvent::RelayConfig(config) => {
                info!(
                    relay_bps = config.relay_bps,
                    merged_builder_bps = config.merged_builder_bps,
                    winning_builder_bps = config.winning_builder_bps,
                    collaterals = config.builder_collaterals.len(),
                    "relay config received"
                );
                self.relay = Some(config);
            }
            EngineEvent::SlotStart(msg) => self.start_slot(msg),
            EngineEvent::SlotEnd { slot } => {
                if self.slot.as_ref().is_some_and(|s| s.slot == slot) {
                    self.end_slot("slot_end");
                }
            }
            EngineEvent::MergeableBlock { body, recv_ns, generation } => {
                if generation == self.generation {
                    self.ingest(&body, recv_ns);
                }
            }
        }
    }

    fn start_slot(&mut self, msg: SlotStartV1) {
        if let Some(slot) = &self.slot &&
            (msg.slot < slot.slot ||
                (msg.slot == slot.slot && msg.parent_hash == slot.parent_hash))
        {
            return;
        }
        self.end_slot("slot_start");
        // Without a distribution policy nothing can be merged; blocks reject as stale.
        let Some(relay) = self.relay.clone() else { return };
        let recorder = self
            .config
            .record
            .as_ref()
            .filter(|record| msg.slot.is_multiple_of(record.every_n_slots))
            .map(|record| {
                let signer = eaddr(self.config.relay_signer.address());
                let safes = relay.builder_collaterals.iter().map(|c| eaddr(c.collateral_safe));
                let recorder = SlotRecorder::new(record, msg.slot, signer, safes.collect());
                let now = utcnow_ns();
                recorder.event(now, &EngineEvent::RelayConfig(relay.clone()));
                recorder.event(now, &EngineEvent::SlotStart(msg));
                recorder
            });
        debug!(slot = msg.slot, parent_hash = %msg.parent_hash, "slot start");
        let (pool, builders) = std::mem::take(&mut self.spare);
        self.slot = Some(Slot {
            slot: msg.slot,
            parent_hash: msg.parent_hash,
            proposer: msg.proposer_fee_recipient,
            beacon_root: msg.parent_beacon_block_root,
            blacklist: msg.apply_blacklist,
            relay,
            pool,
            parent: None,
            builders,
            next: 0,
            recorder,
        });
    }

    fn end_slot(&mut self, reason: &str) {
        let Some(slot) = self.slot.take() else { return };
        if let Some(recorder) = slot.recorder {
            recorder.finish();
        }
        let (blocks, orders) = (slot.pool.blocks.len(), slot.pool.orders.len());
        let builders = slot.builders.len();
        info!(reason, slot = slot.slot, blocks, orders, builders, "merge slot ended");
        metrics::slot_pool(blocks, orders, builders);
        let (mut pool, mut builders) = (slot.pool, slot.builders);
        pool.clear();
        builders.clear();
        self.spare = (pool, builders);
        self.nodes.get_mut().trim();
    }

    fn ingest(&mut self, body: &[u8], recv_ns: u64) {
        let start = std::time::Instant::now();
        let Some(slot) = self.slot.as_mut() else {
            let subject = RejectSubject::None(0);
            let msg = b"stale slot".to_vec();
            let msg = RejectV1 { slot: 0, code: RejectCode::StaleSlot, subject, msg };
            let _unused = self.out.send(EngineOutput::Reject { generation: self.generation, msg });
            metrics::rejection("ingest", "stale_slot");
            return;
        };
        let ingested = slot.pool.block(
            body,
            recv_ns,
            slot.slot,
            slot.parent_hash,
            self.config.max_blocks_per_slot,
            self.config.max_orders_per_slot,
        );
        metrics::stage_latency("merge_ingest", start.elapsed().as_micros() as u64);
        if let Some(reject) = ingested.reject {
            metrics::rejection("ingest", reject.label);
            warn!(label = reject.label, msg = reject.msg, "mergeable block rejected");
            let subject =
                reject.block_hash.map_or(RejectSubject::None(0), RejectSubject::BlockHash);
            let msg = RejectV1 {
                slot: slot.slot,
                code: reject.code,
                subject,
                msg: reject.msg.into_bytes(),
            };
            let _unused = self.out.send(EngineOutput::Reject { generation: self.generation, msg });
        }
        let Some(base) = ingested.base else { return };
        let coinbase = aaddr(base.header.coinbase);
        match slot.builders.iter().position(|b| b.coinbase == coinbase) {
            // A newer base supersedes one not yet opened.
            Some(i) => slot.builders[i].pending = Some(base),
            None if slot.builders.len() < self.config.max_builders => {
                slot.builders.push(Builder { coinbase, pending: Some(base), merge: None });
            }
            None => metrics::rejection("merge", "max_builders"),
        }
    }

    /// One unit of merge work; false when there was none.
    fn step(&mut self) -> bool {
        let Some(slot) = self.slot.as_mut() else { return false };
        let now = utcnow_ns();
        let max_age_ns = self.config.max_base_age.as_nanos() as u64;
        let Some(i) = slot.pick(now, max_age_ns) else { return false };
        slot.next = (i + 1) % slot.builders.len();
        if slot.parent.is_none() {
            match Parent::open(&self.store, slot.parent_hash, slot.recorder.as_ref()) {
                Ok(parent) => slot.parent = Some(parent),
                Err(err) => {
                    // Nothing can run until the node has the parent; try again on the next base.
                    debug!(%err, "parent state not ready");
                    metrics::rejection("merge", "parent_not_ready");
                    slot.builders[i].pending = None;
                    return true;
                }
            }
        }
        let Some(parent) = slot.parent.as_ref() else { return true };
        let cx = Cx {
            store: &self.store,
            parent,
            nodes: &self.nodes,
            relay: &slot.relay,
            slot: slot.slot,
            proposer: slot.proposer,
            beacon_root: slot.beacon_root,
            blacklist: slot.blacklist.then_some(&*self.config.disallow),
            signer: &self.config.relay_signer,
            min_increase: self.config.min_value_increase_wei,
        };
        let builder = &mut slot.builders[i];
        let warming = self.config.warmup && builder.merge.is_none() && builder.pending.is_some();
        let start = std::time::Instant::now();
        if let Some(base) = builder.pending.take() {
            let hash = base.hash;
            let merge = builder.merge.get_or_insert_with(Merge::new);
            if let Err(reason) = merge.open(base, &slot.pool, &cx, &mut self.scratch) {
                metrics::rejection("merge_open", reason);
                debug!(base = %hash, reason, "base not merged");
                return true;
            }
            if !warming {
                metrics::stage_latency("merge_open", start.elapsed().as_micros() as u64);
            }
        }
        let Some(merge) = builder.merge.as_mut().filter(|m| m.live) else { return true };
        let extend_start = std::time::Instant::now();
        merge.extend(&slot.pool, &cx, &mut self.scratch);
        let emit_start = std::time::Instant::now();
        let emitted = merge.emit(&slot.pool, &cx, &mut self.scratch);
        if warming {
            debug!(base = %merge.base.hash, builder = %builder.coinbase, "base warmed");
            merge.live = false;
            return true;
        }
        metrics::stage_latency("merge_extend", (emit_start - extend_start).as_micros() as u64);
        match emitted {
            Ok(Some(msg)) => {
                let emitted = utcnow_ns();
                if merge.emissions == 1 {
                    let age_us = emitted.saturating_sub(merge.base.recv_ns) / 1000;
                    metrics::stage_latency("merge_first_emit", age_us);
                }
                metrics::stage_latency("merge_emit", emit_start.elapsed().as_micros() as u64);
                info!(
                    base = %merge.base.hash,
                    builder = %builder.coinbase,
                    emission = merge.emissions,
                    base_age_us = emitted.saturating_sub(merge.base.recv_ns) / 1000,
                    step_us = start.elapsed().as_micros() as u64,
                    orders = msg.included_order_ids.len(),
                    proposer_value = %msg.proposer_value,
                    "merged block emitted"
                );
                let msg = EngineOutput::Merged { generation: self.generation, msg: Box::new(msg) };
                let _unused = self.out.send(msg);
            }
            Ok(None) => {}
            Err(reason) => {
                metrics::rejection("merge_emit", reason);
                debug!(base = %merge.base.hash, reason, "base dropped");
                merge.live = false;
            }
        }
        true
    }
}
