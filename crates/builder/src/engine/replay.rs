//! Speculative base-block replay. Every appendable block from a candidate
//! builder is replayed onto parent state as it arrives, so the live session can
//! move onto a fresh base instead of ageing on the one the relay activated.
//!
//! Jobs shard on the base block's beneficiary, so one builder's blocks always
//! land on the same worker and its `ReplayCheckpoint` chain extends in order.
//!
//! Each builder gets a single pending slot rather than a queue: a newer block
//! replaces an older one that has not started yet. Replaying a block the
//! builder has already superseded cannot help -- only the newest base can
//! produce a merge that beats that builder's own latest bid -- so a backlog is
//! pure staleness.

use std::sync::{Arc, Condvar, Mutex};

use alloy_primitives::{Address, B256};
use crossbeam_channel::Sender;
use ethrex_blockchain::Blockchain;
use ethrex_storage::Store;
use helix_tcp_types::merging::control::RelayConfigV1;
use rustc_hash::FxHashMap;
use tracing::{debug, info, warn};

use crate::engine::{
    EngineEvent,
    session::{MergeSession, ReplayCheckpoint},
    types::{MAX_REPLAY_CHECKPOINTS, PreparedBlock, SlotContext},
};

pub struct ReplayJob {
    pub ctx: Arc<SlotContext>,
    pub base: Arc<PreparedBlock>,
    pub relay_config: Arc<RelayConfigV1>,
    pub generation: u64,
}

/// Outcome of offering a job to the pool.
pub enum Dispatch {
    Accepted,
    /// Replaced an older pending job for the same builder. That block will
    /// never be replayed, so the caller must stop treating it as in flight.
    Superseded(B256),
    /// Refused; the caller falls back to an inline replay on activation.
    Refused,
}

/// Pending work for one worker: at most one job per builder, newest wins.
pub(crate) struct Pending {
    jobs: Mutex<FxHashMap<Address, ReplayJob>>,
    signal: Condvar,
    max_builders: usize,
}

impl Pending {
    pub(crate) fn new(max_builders: usize) -> Self {
        Self { jobs: Mutex::new(FxHashMap::default()), signal: Condvar::new(), max_builders }
    }

    /// Stores `job`, replacing any pending job for the same builder.
    pub(crate) fn offer(&self, beneficiary: Address, job: ReplayJob) -> Dispatch {
        let Ok(mut jobs) = self.jobs.lock() else { return Dispatch::Refused };
        if !jobs.contains_key(&beneficiary) && jobs.len() >= self.max_builders {
            return Dispatch::Refused;
        }
        match jobs.insert(beneficiary, job) {
            Some(old) => Dispatch::Superseded(old.base.block_hash),
            None => Dispatch::Accepted,
        }
    }

    /// Takes the pending job with the highest bid, if any. The builder most
    /// likely to win the slot is the one worth warming first.
    pub(crate) fn try_take(&self) -> Option<ReplayJob> {
        let mut jobs = self.jobs.lock().ok()?;
        let builder =
            jobs.iter().max_by_key(|(_, job)| job.base.block_value).map(|(builder, _)| *builder)?;
        jobs.remove(&builder)
    }
}

/// Fixed pool of replay workers, addressed by beneficiary.
pub struct ReplayPool {
    shards: Vec<Arc<Pending>>,
}

impl ReplayPool {
    pub fn spawn(
        workers: usize,
        max_builders: usize,
        cores: &[usize],
        store: Store,
        blockchain: Arc<Blockchain>,
        out: Sender<EngineEvent>,
    ) -> Self {
        let mut shards = Vec::with_capacity(workers);
        for id in 0..workers {
            let pending = Arc::new(Pending {
                jobs: Mutex::new(FxHashMap::default()),
                signal: Condvar::new(),
                max_builders,
            });
            shards.push(pending.clone());
            let core = cores.get(id).copied();
            let store = store.clone();
            let blockchain = blockchain.clone();
            let out = out.clone();
            std::thread::Builder::new()
                .name(format!("merge-replay-{id}"))
                .spawn(move || {
                    if let Some(core) = core &&
                        !core_affinity::set_for_current(core_affinity::CoreId { id: core })
                    {
                        warn!(core, id, "failed to pin replay worker");
                    }
                    run_worker(id, pending, store, blockchain, out);
                })
                .expect("failed to spawn replay worker");
        }
        info!(workers, max_builders, "speculative replay pool started");
        Self { shards }
    }

    /// Offers a job, replacing any pending job for the same builder. Never
    /// blocks the engine thread.
    pub fn dispatch(&self, beneficiary: Address, job: ReplayJob) -> Dispatch {
        let shard = &self.shards[shard_of(&beneficiary, self.shards.len())];
        let outcome = shard.offer(beneficiary, job);
        shard.signal.notify_one();
        outcome
    }
}

fn shard_of(beneficiary: &Address, workers: usize) -> usize {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&beneficiary.as_slice()[12..20]);
    (u64::from_le_bytes(bytes) % workers as u64) as usize
}

/// Blocks until a job is pending, then takes the highest-bid one.
fn take_next(pending: &Pending) -> Option<ReplayJob> {
    loop {
        if let Some(job) = pending.try_take() {
            return Some(job);
        }
        let jobs = pending.jobs.lock().ok()?;
        // Re-check under the lock: a job may have landed since `try_take`.
        if !jobs.is_empty() {
            drop(jobs);
            continue;
        }
        let _unused = pending.signal.wait(jobs).ok()?;
    }
}

fn run_worker(
    id: usize,
    pending: Arc<Pending>,
    store: Store,
    blockchain: Arc<Blockchain>,
    out: Sender<EngineEvent>,
) {
    let mut slot = 0u64;
    let mut checkpoints: FxHashMap<Address, ReplayCheckpoint> = FxHashMap::default();
    while let Some(job) = take_next(&pending) {
        // Checkpoints are only valid within one slot's fixed parent/timestamp.
        if job.ctx.slot != slot {
            if job.ctx.slot < slot {
                continue;
            }
            slot = job.ctx.slot;
            checkpoints.clear();
        }

        let beneficiary = job.base.beneficiary();
        let block_hash = job.base.block_hash;
        let result = MergeSession::activate(
            &job.ctx,
            &job.base,
            &store,
            blockchain.clone(),
            &job.relay_config,
            checkpoints.get(&beneficiary),
        );

        let event = match result {
            Ok((session, checkpoint, checkpoint_hit)) => {
                if checkpoints.len() >= MAX_REPLAY_CHECKPOINTS &&
                    !checkpoints.contains_key(&beneficiary) &&
                    let Some(evict) = checkpoints.keys().next().copied()
                {
                    checkpoints.remove(&evict);
                }
                checkpoints.insert(beneficiary, checkpoint);
                EngineEvent::Prebuilt {
                    slot: job.ctx.slot,
                    generation: job.generation,
                    block_hash,
                    beneficiary,
                    checkpoint_hit,
                    session: Some(Box::new(session)),
                }
            }
            Err(err) => {
                debug!(worker = id, %block_hash, %err, "speculative replay failed");
                EngineEvent::Prebuilt {
                    slot: job.ctx.slot,
                    generation: job.generation,
                    block_hash,
                    beneficiary,
                    checkpoint_hit: false,
                    session: None,
                }
            }
        };
        if out.send(event).is_err() {
            debug!(worker = id, %block_hash, "engine gone, stopping replay worker");
            return;
        }
    }
}
