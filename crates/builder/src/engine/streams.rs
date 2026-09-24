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

    /// The bid of the block waiting for this builder, if any. The stream
    /// weighs that against what another improvement pass is worth before
    /// giving up the base it holds.
    pub(crate) fn waiting_bid(&self) -> Option<U256> {
        let pending = self.pending.lock().ok()?;
        pending.as_ref().map(|job| job.base.block_value)
    }

    #[cfg(test)]
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
    // Delta the previous base ended on, so the next one can report how much of
    // it a single pass recovers.
    let mut prior_delta: Option<U256> = None;
    let mut was_busy = false;
    while let Some(job) = stream.take_blocking() {
        metrics::worker_wait(
            if was_busy { "stream_busy" } else { "stream_idle" },
            utcnow_ns().saturating_sub(job.offered_ns) / 1_000_000,
        );
        was_busy = true;
        // Checkpoints are only valid within one slot's fixed parent/timestamp.
        if job.shared.ctx.slot != slot {
            if job.shared.ctx.slot < slot {
                continue;
            }
            slot = job.shared.ctx.slot;
            checkpoint = None;
            prior_delta = None;
        }
        match merge_base(id, &job, &stream, prior_delta, &store, &blockchain, &mut checkpoint, &out)
        {
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
        was_busy = stream.waiting_bid().is_some();
    }
    info!(worker = id, %beneficiary, "merge stream stopped");
}

/// One base: replay it, then keep improving it until a waiting block is worth
/// more than another pass would add.
///
/// Emitting once and idling costs more than it saves -- a single pass leaves
/// real value in the pool -- but so does holding a base the builder has bid
/// well past. The decision is made on value, not elapsed time: staleness only
/// ever mattered through the bid drift it stands for.
#[allow(clippy::too_many_arguments)]
fn merge_base(
    id: usize,
    job: &StreamJob,
    stream: &BuilderStream,
    prior_delta: Option<U256>,
    store: &Store,
    blockchain: &Arc<Blockchain>,
    checkpoint: &mut Option<ReplayCheckpoint>,
    out: &Sender<EngineOutput>,
) -> Result<U256, crate::engine::error::MergeError> {
    let shared = &job.shared;

    let (mut session, fresh_checkpoint, checkpoint_hit) = MergeSession::activate(
        &shared.ctx,
        &job.base,
        store,
        blockchain.clone(),
        &shared.relay_config,
        checkpoint.as_ref(),
    )
    .inspect_err(|err| metrics::rejection("merge_cycle", err.metric_label()))?;
    *checkpoint = Some(fresh_checkpoint);
    metrics::speculation(if checkpoint_hit { "checkpoint_hit" } else { "checkpoint_miss" });
    session.timeline.dispatch_ns = job.offered_ns;
    // Set after the replay, so `to_emit` measures the work after it rather
    // than overlapping it.
    session.timeline.live_ns = utcnow_ns();

    // Rebase on value, not on age. A newer base is only worth taking when the
    // bid it carries gains more than another improvement pass on the base we
    // hold would -- switching forfeits the delta already accumulated and only
    // recovers part of it on the first pass of the new base.
    let mut passes = 0u64;
    let mut published = session.base_bid_value;
    // Until a pass has run there is no marginal estimate, so nothing short of a
    // clearly better base should displace this one.
    let mut last_gain = U256::MAX;
    let mut emitted_any = false;
    let mut seen_version = u64::MAX;
    loop {
        // Extend only against a pool that has actually moved. Re-screening an
        // unchanged pool yields the same result and holds the read lock that
        // ingest needs to write.
        let version = shared.pool_version.load(Ordering::Acquire);
        let pool_moved = version != seen_version;
        if pool_moved {
            seen_version = version;
            passes += 1;
            {
                let lock_start = std::time::Instant::now();
                let inner = shared.inner.read().expect("shared slot poisoned");
                metrics::stage_latency("extend_lock_wait", lock_start.elapsed().as_micros() as u64);
                session.try_extend(&inner.orders, &inner.excluded);
            }
            match session.emit(
                shared.ctx.slot,
                shared.ctx.proposer_fee_recipient,
                &shared.relay_config,
                &shared.engine_config,
            ) {
                Ok(EmitOutcome::Emitted(msg)) => {
                    last_gain = msg.proposer_value.saturating_sub(published);
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
                    }
                    report_emission(id, job, &session, &msg, checkpoint_hit, passes);
                    if out.send(EngineOutput::Merged { generation: job.generation, msg }).is_err() {
                        return Ok(published.saturating_sub(session.base_bid_value));
                    }
                }
                // Nothing left to add: any positive base gain is worth taking.
                Ok(EmitOutcome::NotImproved) => last_gain = U256::ZERO,
                // An improvement is waiting on emission spacing, not absent.
                Ok(EmitOutcome::Throttled) => {}
                Err(err) => {
                    metrics::rejection("emission", err.metric_label());
                    session.log_stats("emit_failed");
                    return Err(err);
                }
            }
        }

        if let Some(next_bid) = stream.waiting_bid() {
            // Switching forfeits the delta accumulated here and recovers only
            // part of it on the first pass of the new base, so a waiting bid
            // has to beat that forfeit as well as the next pass's gain.
            let delta = published.saturating_sub(session.base_bid_value);
            let forfeit = delta
                .saturating_mul(U256::from(10_000u64 - shared.engine_config.rebase_recovery_bps)) /
                U256::from(10_000u64);
            let base_gain = next_bid.saturating_sub(session.base_bid_value);
            if base_gain > last_gain.saturating_add(forfeit) {
                metrics::rebase_reason("base_gain_wins");
                break;
            }
            metrics::rebase_reason("holding");
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
            stream.wait_for_change(POLL);
        }
    }

    let final_delta = published.saturating_sub(session.base_bid_value);
    metrics::extend_passes(passes);
    session.log_stats("base_done");
    Ok(final_delta)
}

/// The exact comparison the relay makes at get_header, run here while every
/// term is still in hand.
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
        "merged block emitted"
    );
}
