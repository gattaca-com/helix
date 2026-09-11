//! Speculative base-block replay. Every appendable block is replayed onto
//! parent state as it arrives, so `ActivateBaseBlockV1` costs a map lookup
//! instead of the ~84ms replay the engine thread used to pay inline.
//!
//! Jobs shard on the base block's beneficiary, so one builder's blocks always
//! land on the same worker and its `ReplayCheckpoint` chain extends in order.

use std::sync::Arc;

use alloy_primitives::Address;
use crossbeam_channel::{Receiver, Sender, TrySendError};
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

/// Fixed pool of replay workers, addressed by beneficiary.
pub struct ReplayPool {
    senders: Vec<Sender<ReplayJob>>,
}

impl ReplayPool {
    pub fn spawn(
        workers: usize,
        queue_capacity: usize,
        cores: &[usize],
        store: Store,
        blockchain: Arc<Blockchain>,
        out: Sender<EngineEvent>,
    ) -> Self {
        let mut senders = Vec::with_capacity(workers);
        for id in 0..workers {
            let (tx, rx) = crossbeam_channel::bounded(queue_capacity);
            senders.push(tx);
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
                    run_worker(id, rx, store, blockchain, out);
                })
                .expect("failed to spawn replay worker");
        }
        info!(workers, queue_capacity, "speculative replay pool started");
        Self { senders }
    }

    /// Queues a replay, or reports the worker queue was full. Never blocks the
    /// engine thread: a dropped job just costs an inline replay on activation.
    pub fn dispatch(&self, beneficiary: Address, job: ReplayJob) -> bool {
        let shard = shard_of(&beneficiary, self.senders.len());
        match self.senders[shard].try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn shard_of(beneficiary: &Address, workers: usize) -> usize {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&beneficiary.as_slice()[12..20]);
    (u64::from_le_bytes(bytes) % workers as u64) as usize
}

fn run_worker(
    id: usize,
    jobs: Receiver<ReplayJob>,
    store: Store,
    blockchain: Arc<Blockchain>,
    out: Sender<EngineEvent>,
) {
    let mut slot = 0u64;
    let mut checkpoints: FxHashMap<Address, ReplayCheckpoint> = FxHashMap::default();
    while let Ok(job) = jobs.recv() {
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
