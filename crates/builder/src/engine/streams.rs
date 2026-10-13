//! Per-builder merge streams.
//!
//! We cannot know which builder will hold the top bid when `get_header` is
//! called, so every candidate builder is treated as an independent stream of
//! work. Each stream builds the builder's newest appendable block, extends it
//! with the pooled orders, emits, then goes straight back for that builder's
//! newest block again.
//!
//! Streams shard on the beneficiary, so one builder's blocks always land on the
//! same worker and each builds on the last. Each builder has a single pending
//! slot: a newer block replaces one that has not started, because a base the
//! builder has already superseded cannot beat their latest bid.

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Bound on how long a stream sleeps with nothing to do. Only sets how late the
/// staleness backstop can fire; real work arrives by wake-up.
pub(crate) const POLL: std::time::Duration = std::time::Duration::from_millis(5);

use alloy_primitives::Address;
use crossbeam_channel::Sender;
use ethrex_storage::Store;
use rustc_hash::FxHashMap;
use tracing::{debug, info, warn};

use crate::{
    engine::{
        EngineOutput,
        builder::Builder,
        types::{PreparedBlock, SharedSlot},
    },
    metrics,
};

pub struct StreamJob {
    pub shared: Arc<SharedSlot>,
    pub base: Arc<PreparedBlock>,
    pub generation: u64,
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
    pub(crate) fn wait_for_change(&self, timeout: std::time::Duration) {
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
        out: Sender<EngineOutput>,
    ) -> Self {
        info!(max_streams, "merge streams ready");
        Self {
            streams: Mutex::new(FxHashMap::default()),
            max_streams,
            cores: cores.to_vec(),
            store,
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
                let out = self.out.clone();
                std::thread::Builder::new()
                    .name(format!("merge-stream-{id}"))
                    .stack_size(crate::engine::builder::EVM_STACK)
                    .spawn(move || {
                        if let Some(core) = core &&
                            !core_affinity::set_for_current(core_affinity::CoreId { id: core })
                        {
                            warn!(core, id, "failed to pin merge stream");
                        }
                        run_stream(id, worker, store, out);
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

fn run_stream(id: usize, stream: Arc<BuilderStream>, store: Store, out: Sender<EngineOutput>) {
    let mut builder = Builder::default();
    while let Some(job) = stream.take_blocking() {
        if let Err(err) = builder.merge(id, &job, &stream, &store, &out) {
            metrics::rejection("merge_cycle", err.metric_label());
            debug!(worker = id, base_block_hash = %job.base.block_hash, %err, "merge cycle failed");
        }
    }
    info!(worker = id, "merge stream stopped");
}
