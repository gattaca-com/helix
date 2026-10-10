//! Per-builder merge streams.
//!
//! We cannot know which builder will hold the top bid when `get_header` is
//! called, so every candidate builder is treated as an independent stream of
//! work. Each stream runs the whole cycle -- replay the builder's newest
//! appendable block, extend it with the pooled orders, emit -- then goes
//! straight back for that builder's newest block again.
//!
//! Nothing is pre-warmed and nothing is cached between cycles. Measurement
//! showed the base age at emission was dominated not by the replay but by a
//! session sitting live while it accumulated value, so the cheapest way to
//! keep a base fresh is to stop holding onto it. A warm-session cache can come
//! back if the data asks for it.
//!
//! Streams shard on the beneficiary, so one builder's blocks always land on the
//! same worker and its `ReplayCheckpoint` chain extends in order. Each builder
//! has a single pending slot: a newer block replaces one that has not started,
//! because a base the builder has already superseded cannot beat their latest
//! bid.

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

use flux_profiler::timed;

/// Bound on how long a stream sleeps with nothing to do. Only sets how late the
/// staleness backstop can fire; real work arrives by wake-up.
const POLL: std::time::Duration = std::time::Duration::from_millis(5);

use alloy_primitives::{Address, U256};
use crossbeam_channel::Sender;
use ethrex_blockchain::Blockchain;
use ethrex_storage::Store;
use rustc_hash::FxHashMap;
use tracing::{debug, info, warn};

use crate::{
    engine::{
        EngineOutput,
        session::{EmitOutcome, MergeSession, ReplayCheckpoint},
        state_layer::StateLayer,
        types::{PreparedBlock, SharedSlot},
    },
    metrics,
    utils::utcnow_ns,
};

pub struct StreamJob {
    pub shared: Arc<SharedSlot>,
    pub base: Arc<PreparedBlock>,
    pub generation: u64,
    /// When the engine offered this block, so the wait before it starts is
    /// measurable.
    pub offered_ns: u64,
}

/// Outcome of offering a block to a stream.
pub enum Offer {
    Accepted,
    /// Replaced a block that had not started. It will never be merged.
    Superseded,
    /// Refused; more distinct builders pending than the stream will hold.
    Refused,
}

/// One builder's stream. A single pending slot, newest wins: a base the
/// builder has already superseded cannot beat their latest bid, so replaying it
/// is pure staleness.
pub(crate) struct BuilderStream {
    pending: Mutex<Option<StreamJob>>,
    signal: Condvar,
    stopped: AtomicBool,
}

impl BuilderStream {
    pub(crate) fn new() -> Self {
        Self { pending: Mutex::new(None), signal: Condvar::new(), stopped: AtomicBool::new(false) }
    }

    pub(crate) fn offer(&self, job: StreamJob) -> Offer {
        let Ok(mut pending) = self.pending.lock() else { return Offer::Refused };
        let outcome = if pending.is_some() { Offer::Superseded } else { Offer::Accepted };
        *pending = Some(job);
        drop(pending);
        self.signal.notify_one();
        outcome
    }

    /// Whether a newer block from this builder is waiting to replace the base
    /// the stream holds.
    pub(crate) fn has_waiting(&self) -> bool {
        self.pending.lock().is_ok_and(|pending| pending.is_some())
    }

    pub(crate) fn try_take(&self) -> Option<StreamJob> {
        self.pending.lock().ok()?.take()
    }

    fn take_blocking(&self) -> Option<StreamJob> {
        let mut pending = self.pending.lock().ok()?;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            if let Some(job) = pending.take() {
                return Some(job);
            }
            pending = self.signal.wait(pending).ok()?;
        }
    }

    /// Blocks until a new base is offered, the pool changes, or `timeout`.
    fn wait_for_change(&self, timeout: std::time::Duration) {
        let Ok(pending) = self.pending.lock() else { return };
        let _unused = self.signal.wait_timeout(pending, timeout);
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.signal.notify_all();
    }
}

/// One stream per builder, created the first time that builder offers an
/// appendable block. Builders are bounded by the relay's collateral set -- a
/// handful -- so no builder ever queues behind another.
pub struct MergeStreams {
    streams: Mutex<FxHashMap<Address, Arc<BuilderStream>>>,
    max_streams: usize,
    cores: Vec<usize>,
    store: Store,
    blockchain: Arc<Blockchain>,
    out: Sender<EngineOutput>,
}

impl Drop for MergeStreams {
    fn drop(&mut self) {
        if let Ok(streams) = self.streams.lock() {
            for stream in streams.values() {
                stream.stop();
            }
        }
    }
}

impl MergeStreams {
    pub fn new(
        max_streams: usize,
        cores: &[usize],
        store: Store,
        blockchain: Arc<Blockchain>,
        out: Sender<EngineOutput>,
    ) -> Self {
        info!(max_streams, "merge streams ready");
        Self {
            streams: Mutex::new(FxHashMap::default()),
            max_streams,
            cores: cores.to_vec(),
            store,
            blockchain,
            out,
        }
    }

    /// Wakes every stream, so a pool change is picked up without waiting out a
    /// poll interval.
    pub fn wake_all(&self) {
        if let Ok(streams) = self.streams.lock() {
            for stream in streams.values() {
                stream.signal.notify_all();
            }
        }
    }

    /// Offers a block to that builder's stream, starting one if this is the
    /// first block from them. Never blocks the engine thread.
    pub fn offer(&self, beneficiary: Address, job: StreamJob) -> Offer {
        let Ok(mut streams) = self.streams.lock() else { return Offer::Refused };
        let stream = match streams.get(&beneficiary) {
            Some(stream) => stream.clone(),
            None => {
                if streams.len() >= self.max_streams {
                    return Offer::Refused;
                }
                let stream = Arc::new(BuilderStream::new());
                let id = streams.len();
                let core = self.cores.get(id).copied();
                let worker = stream.clone();
                let store = self.store.clone();
                let blockchain = self.blockchain.clone();
                let out = self.out.clone();
                std::thread::Builder::new()
                    .name(format!("merge-stream-{id}"))
                    .spawn(move || {
                        if let Some(core) = core &&
                            !core_affinity::set_for_current(core_affinity::CoreId { id: core })
                        {
                            warn!(core, id, "failed to pin merge stream");
                        }
                        run_stream(id, beneficiary, worker, store, blockchain, out);
                    })
                    .expect("failed to spawn merge stream");
                info!(%beneficiary, id, "merge stream started for builder");
                streams.insert(beneficiary, stream.clone());
                stream
            }
        };
        drop(streams);
        stream.offer(job)
    }
}

/// What the stream has been doing since when, so a waiting base's wait can be split by it.
#[derive(Default)]
struct Phases(Vec<(&'static str, u64)>);

impl Phases {
    fn enter(&mut self, phase: &'static str) {
        self.0.push((phase, utcnow_ns()));
    }

    /// Splits `[from, to)` over the phases it overlapped, then starts afresh.
    fn attribute(&mut self, from: u64, to: u64) {
        let mut total = 0;
        let mut dominant = ("none", 0);
        for (i, &(phase, start)) in self.0.iter().enumerate() {
            let end = self.0.get(i + 1).map_or(to, |next| next.1);
            let overlap = end.min(to).saturating_sub(start.max(from));
            if overlap == 0 {
                continue;
            }
            metrics::wait_phase(phase, overlap);
            total += overlap;
            if overlap > dominant.1 {
                dominant = (phase, overlap);
            }
        }
        if total >= 5_000_000 {
            metrics::wait_dominant(dominant.0);
        }
        self.0.clear();
    }
}

fn run_stream(
    id: usize,
    beneficiary: Address,
    stream: Arc<BuilderStream>,
    store: Store,
    blockchain: Arc<Blockchain>,
    out: Sender<EngineOutput>,
) {
    let mut slot = 0u64;
    let mut checkpoint: Option<ReplayCheckpoint> = None;
    let mut layer: Option<StateLayer> = None;
    // Delta the previous base ended on, so the next one can report how much of
    // it a single pass recovers.
    let mut prior_delta: Option<U256> = None;
    let mut was_busy = false;
    // One coinbase can submit from several pubkeys, each its own stream of blocks; with
    // `warmup_per_pubkey` each warms up separately before its bases are merged.
    let mut bases_in_slot: FxHashMap<alloy_rpc_types::beacon::BlsPublicKey, u32> =
        Default::default();
    let mut phases = Phases::default();
    phases.enter("idle");
    while let Some(job) = stream.take_blocking() {
        // Checkpoints are only valid within one slot's fixed parent/timestamp.
        if job.shared.ctx.slot != slot {
            if job.shared.ctx.slot < slot {
                continue;
            }
            slot = job.shared.ctx.slot;
            checkpoint = None;
            layer = None;
            prior_delta = None;
            bases_in_slot.clear();
        }
        let stream_key = if job.shared.engine_config.warmup_per_pubkey {
            job.base.builder_pubkey
        } else {
            Default::default()
        };
        let seen = bases_in_slot.entry(stream_key).or_default();
        let warming = *seen < job.shared.engine_config.warmup_bases;
        *seen += 1;
        metrics::set_warming(warming);
        phases.attribute(job.offered_ns, utcnow_ns());
        metrics::worker_wait(
            if was_busy { "stream_busy" } else { "stream_idle" },
            utcnow_ns().saturating_sub(job.offered_ns) / 1_000_000,
        );
        match merge_base(
            id,
            &job,
            &stream,
            warming,
            prior_delta,
            &store,
            &blockchain,
            &mut checkpoint,
            &mut layer,
            &mut phases,
            &out,
        ) {
            Ok(delta) => prior_delta = Some(delta),
            Err(err) => {
                debug!(
                    worker = id,
                    %beneficiary,
                    base_block_hash = %job.base.block_hash,
                    %err,
                    "merge cycle failed"
                );
            }
        }
        // A base that arrived during warm-up waited behind it; the next one starts clean.
        if warming {
            stream.try_take();
        }
        was_busy = stream.has_waiting();
        phases.enter("idle");
    }
    info!(worker = id, %beneficiary, "merge stream stopped");
}

/// One base: replay it, then keep improving it until the builder sends a newer
/// one.
///
/// A newer base can drop txs the builder cancelled or add ones it
/// preconfirmed, so merging on a superseded base can break the builder's
/// commitments however much it pays. Each cycle still runs to an emission
/// before switching: when bases arrive faster than a replay, breaking off
/// would never emit at all.
#[allow(clippy::too_many_arguments)]
#[timed]
fn merge_base(
    id: usize,
    job: &StreamJob,
    stream: &BuilderStream,
    warming: bool,
    prior_delta: Option<U256>,
    store: &Store,
    blockchain: &Arc<Blockchain>,
    checkpoint: &mut Option<ReplayCheckpoint>,
    layer: &mut Option<StateLayer>,
    phases: &mut Phases,
    out: &Sender<EngineOutput>,
) -> Result<U256, crate::engine::error::MergeError> {
    let shared = &job.shared;
    phases.enter("replay");

    let (mut session, fresh_checkpoint, checkpoint_hit) = MergeSession::activate(
        &shared.ctx,
        &job.base,
        store,
        blockchain.clone(),
        &shared.relay_config,
        checkpoint.as_ref(),
        layer.take(),
    )
    .inspect_err(|err| metrics::rejection("merge_cycle", err.metric_label()))?;
    *checkpoint = Some(fresh_checkpoint);
    metrics::speculation(if checkpoint_hit { "checkpoint_hit" } else { "checkpoint_miss" });
    session.timeline.dispatch_ns = job.offered_ns;
    // Set after the replay, so `to_emit` measures the work after it rather
    // than overlapping it.
    session.timeline.live_ns = utcnow_ns();

    let mut passes = 0u64;
    let mut published = session.base_bid_value;
    let mut emitted_any = false;
    let mut seen_version = u64::MAX;
    loop {
        // Extend only against a pool that has actually moved. Re-screening an
        // unchanged pool yields the same result and holds the read lock that
        // ingest needs to write.
        let version = shared.pool_version.load(Ordering::Acquire);
        let pool_moved = version != seen_version;
        // Once this base has emitted, a waiting newer one takes priority over
        // more work here: an emission on a superseded base is rarely served.
        if pool_moved && emitted_any && stream.has_waiting() {
            metrics::rebase_reason("newer_base_before_extend");
            break;
        }
        if pool_moved {
            seen_version = version;
            passes += 1;
            phases.enter("extend");
            let candidates = {
                let lock_start = std::time::Instant::now();
                let inner = shared.inner.read().expect("shared slot poisoned");
                metrics::stage_latency("extend_lock_wait", lock_start.elapsed().as_micros() as u64);
                session.screen(&inner.orders, &inner.excluded)
            };
            session.try_extend(candidates);
            if warming {
                break;
            }
            if emitted_any && stream.has_waiting() {
                metrics::rebase_reason("newer_base_before_emit");
                break;
            }
            phases.enter(if emitted_any { "emit_later" } else { "emit_first" });
            match session.emit(
                shared.ctx.slot,
                shared.ctx.proposer_fee_recipient,
                &shared.relay_config,
                &shared.engine_config,
            ) {
                Ok(EmitOutcome::Emitted(msg)) => {
                    published = msg.proposer_value;
                    if !emitted_any {
                        if let Some(previous) = prior_delta {
                            metrics::delta_on_base(
                                "first_pass",
                                msg.proposer_value.saturating_sub(session.base_bid_value),
                            );
                            metrics::delta_on_base("previous_final", previous);
                        }
                        emitted_any = true;
                        session.timeline.first_emit_ns = utcnow_ns();
                        metrics::first_emission(&session.timeline);
                    }
                    report_emission(id, job, &session, &msg, checkpoint_hit, passes);
                    metrics::emit_base(stream.has_waiting());
                    if out.send(EngineOutput::Merged { generation: job.generation, msg }).is_err() {
                        return Ok(published.saturating_sub(session.base_bid_value));
                    }
                }
                Ok(EmitOutcome::NotImproved) => {}
                Err(err) => {
                    metrics::rejection("emission", err.metric_label());
                    session.log_stats("emit_failed");
                    return Err(err);
                }
            }
        }

        // Waiting blocks collapse to the newest, so a burst of submissions
        // skips straight to the latest one.
        if stream.has_waiting() {
            metrics::rebase_reason("newer_base");
            break;
        }
        // Backstop only: past the relay's staleness gate nothing we emit on
        // this base can be served, so further passes are wasted work.
        if utcnow_ns().saturating_sub(job.base.recv_ns) >=
            shared.engine_config.max_base_age.as_nanos() as u64
        {
            metrics::rebase_reason("aged_out");
            break;
        }
        if !pool_moved {
            // Nothing to do until orders arrive or a fresher base does. The
            // timeout only bounds how late the age backstop can fire.
            phases.enter("idle_on_base");
            stream.wait_for_change(POLL);
        }
    }

    phases.enter("layer_handoff");
    *layer = session.take_layer();
    phases.enter("teardown");
    let final_delta = published.saturating_sub(session.base_bid_value);
    metrics::extend_passes(passes);
    session.log_stats(if warming { "warmup" } else { "base_done" });
    Ok(final_delta)
}

/// The exact comparison the relay makes at get_header, run here while every
/// term is still in hand.
#[timed]
fn report_emission(
    id: usize,
    job: &StreamJob,
    session: &MergeSession,
    msg: &helix_tcp_types::merging::builder_to_relay::MergedBlockV1,
    checkpoint_hit: bool,
    passes: u64,
) {
    let beneficiary = job.base.beneficiary();
    let Ok(inner) = job.shared.inner.read() else { return };
    let (own_latest, own_best) = inner.reference_bids(&beneficiary);
    let beats_latest = msg.proposer_value > own_latest;
    metrics::beats_own_bid("latest", beats_latest);
    metrics::beats_own_bid("best", msg.proposer_value > own_best);

    let base_value = session.base_bid_value;
    let base_index = session.base_index;
    let delta = msg.proposer_value.saturating_sub(base_value);
    let uplift = own_latest.saturating_sub(base_value);
    let latest_index = inner.submissions.get(&beneficiary).map(|s| s.count - 1).unwrap_or_default();
    let (budget_steps, budget_ms, deadline_seen) = inner
        .submissions
        .get(&beneficiary)
        .map(|s| s.budget(base_index, base_value, delta))
        .unwrap_or_default();
    let base_age_ms = utcnow_ns().saturating_sub(job.base.recv_ns) / 1_000_000;
    let servable =
        beats_latest && base_age_ms <= job.shared.engine_config.max_base_age.as_millis() as u64;
    metrics::servable_win(servable);
    metrics::emission_phases(&session.timeline);
    metrics::emission_verdict(
        beats_latest,
        latest_index.saturating_sub(base_index),
        base_age_ms,
        budget_steps,
        budget_ms,
        deadline_seen,
    );
    let t = &session.timeline;
    let us = |from: u64, to: u64| to.saturating_sub(from) / 1_000;
    info!(
        worker = id,
        base_block_hash = %msg.base_block_hash,
        builder = %beneficiary,
        base_index,
        latest_index,
        steps_behind = latest_index.saturating_sub(base_index),
        base_age_ms,
        pass = passes,
        delta_gwei = metrics::gwei(delta),
        uplift_gwei = metrics::gwei(uplift),
        budget_steps,
        budget_ms,
        deadline_seen,
        checkpoint_hit,
        won = beats_latest,
        servable,
        ingest_us = us(t.recv_ns, t.ingest_done_ns),
        wait_us = us(t.ingest_done_ns, t.replay_start_ns),
        replay_us = us(t.replay_start_ns, t.replay_end_ns),
        live_us = us(t.replay_end_ns, utcnow_ns()),
        base_txs = session.base_txs,
        checkpoint_shared = session.checkpoint_shared,
        checkpoint_len = session.checkpoint_len,
        "merged block emitted"
    );
}
