use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, atomic::AtomicU64},
    time::Duration,
};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::beacon::BlsPublicKey;
use alloy_signer_local::PrivateKeySigner;
use helix_tcp_types::merging::control::RelayConfigV1;
use rustc_hash::{FxHashMap, FxHashSet};

/// Environment variable holding the relay-owned Safe signer key (same
/// convention as the simulator's `load_signer`).
pub const RELAY_KEY_ENV: &str = "RELAY_KEY";

pub struct EngineConfig {
    /// Safe owner key that signs the distribution tx.
    pub relay_signer: PrivateKeySigner,
    pub max_blocks_per_slot: usize,
    pub max_orders_per_slot: usize,
    /// Emission gate: proposer value must beat the last emission by more than this.
    pub min_value_increase_wei: U256,
    /// Minimum spacing between emissions for the same base.
    pub min_emission_interval: Duration,
    /// Stop improving a base once it is this old. Must match the relay's
    /// `max_merged_bid_age_ms`; it is not sent over the wire.
    pub max_base_age: Duration,
    /// Assumed share of a base's delta that one pass on a new base recovers,
    /// in basis points. The rebase rule charges the remainder against a
    /// waiting bid before taking it.
    pub rebase_recovery_bps: u64,
    /// Optional core pin for the engine worker thread.
    pub core: Option<usize>,
    /// Cap on distinct per-builder merge streams; 0 disables merging.
    pub max_builder_streams: usize,
    /// Warm only the top-K builders by best bid this slot; 0 warms every one.
    pub speculation_top_k: usize,
    /// One core per merge stream, in creation order; empty leaves them unpinned.
    pub replay_worker_cores: Vec<usize>,
}

impl EngineConfig {
    pub fn load_relay_signer() -> PrivateKeySigner {
        let key = std::env::var(RELAY_KEY_ENV)
            .unwrap_or_else(|_| panic!("{RELAY_KEY_ENV} env var not set"));
        key.parse().expect("failed to parse RELAY_KEY")
    }
}

/// A transaction decoded from a forwarded payload, with its recovered sender.
pub struct DecodedTx {
    pub tx: ethrex_common::types::Transaction,
    pub sender: ethrex_common::Address,
    /// keccak of the canonical encoding (wire representation).
    pub hash: B256,
    pub gas_limit: u64,
    pub blob_hashes: Vec<B256>,
}

/// A forwarded `MergeableBlockV1` after decoding and validation.
pub struct PreparedBlock {
    pub block_hash: B256,
    pub builder_pubkey: BlsPublicKey,
    pub block_value: U256,
    pub allow_appending: bool,
    pub payload: alloy_rpc_types::engine::ExecutionPayloadV3,
    /// Index-aligned with `payload.transactions`.
    pub txs: Arc<Vec<Arc<DecodedTx>>>,
    pub recv_ns: u64,
    /// Index of this block in its builder's submission stream this slot: `i`
    /// in the win condition.
    pub submission_index: u64,
    /// When the engine finished decoding and pooling this block.
    pub ingest_done_ns: u64,
}

impl PreparedBlock {
    /// Base block coinbase; the merged block's beneficiary and the shard key
    /// for both replay checkpoints and speculative replay.
    pub fn beneficiary(&self) -> Address {
        self.payload.payload_inner.payload_inner.fee_recipient
    }
}

/// One mergeable order drawn from a prepared block's `merge_orders`.
pub struct PreparedOrder {
    pub order_id: B256,
    pub order_hash: B256,
    pub latest_only: bool,
    pub origin: Address,
    pub builder_pubkey: BlsPublicKey,
    pub source_block_hash: B256,
    pub source_block_value: U256,
    pub txs: Vec<Arc<DecodedTx>>,
    /// Indices into `txs` allowed to revert.
    pub reverting: Vec<usize>,
    /// Indices into `txs` allowed to be omitted, but not revert.
    pub dropping: Vec<usize>,
}

impl PreparedOrder {
    pub fn can_revert(&self, ix: usize) -> bool {
        self.reverting.contains(&ix)
    }

    pub fn can_drop(&self, ix: usize) -> bool {
        self.dropping.contains(&ix)
    }
}

/// Presimulation outcome for a candidate order.
pub struct SimulatedOrder {
    pub order_ix: usize,
    /// Index-aligned with the order's `txs`; false = dropped.
    pub include_tx: Vec<bool>,
    /// Beneficiary balance delta.
    pub builder_payment: U256,
}

/// Revenue attributed to one origin coinbase.
#[derive(Debug, Clone, Default)]
pub struct OriginRevenue {
    pub revenue: U256,
    pub txs: Vec<B256>,
    pub pubkey: BlsPublicKey,
}

/// Consensus-fixed slot fields, shared with the speculative replay workers.
#[derive(Debug, Clone)]
pub struct SlotContext {
    pub slot: u64,
    pub parent_hash: B256,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: B256,
}

/// Slot state shared with the per-builder merge streams. The engine thread
/// owns ingest and writes here; the streams read it to extend and emit, so no
/// part of the merge cycle runs on the engine thread.
pub struct SharedSlot {
    pub ctx: Arc<SlotContext>,
    pub relay_config: Arc<RelayConfigV1>,
    pub engine_config: Arc<EngineConfig>,
    pub inner: RwLock<SharedInner>,
    /// Bumped whenever the pool or the exclusion set changes. A stream reads it
    /// without taking the lock, so an unchanged pool costs nothing: extending
    /// against it would produce the same result while starving ingest of the
    /// write lock.
    pub pool_version: AtomicU64,
}

#[derive(Default)]
pub struct SharedInner {
    pub orders: Vec<PreparedOrder>,
    /// order_id -> index into `orders` (dedup; attribution goes to the
    /// highest-value source block).
    pub order_ids: FxHashMap<B256, usize>,
    pub excluded: FxHashSet<B256>,
    pub latest_only: FxHashMap<BlsPublicKey, FxHashSet<B256>>,
    /// Per-builder bid history. The streams read it at emission to evaluate the
    /// exact comparison the relay will make at get_header.
    pub submissions: FxHashMap<Address, BuilderSubmissions>,
}

impl SharedInner {
    pub fn update_latest_only(&mut self, pubkey: BlsPublicKey, current: FxHashSet<B256>) {
        if let Some(previous) = self.latest_only.get(&pubkey) {
            for dropped in previous.difference(&current) {
                self.excluded.insert(*dropped);
            }
        }
        self.latest_only.insert(pubkey, current);
    }

    #[cfg(test)]
    pub fn is_excluded(&self, order_hash: &B256) -> bool {
        self.excluded.contains(order_hash)
    }

    /// Records a submission and returns its index plus the ratchet against the
    /// previous one from the same builder, as (delta, interval_ms, rising).
    pub fn record_submission(
        &mut self,
        builder: Address,
        value: U256,
        recv_ns: u64,
    ) -> (u64, Option<(U256, u64, bool)>) {
        match self.submissions.get_mut(&builder) {
            Some(prev) => {
                let interval_ms = recv_ns.saturating_sub(prev.last_recv_ns) / 1_000_000;
                let rising = value >= prev.last;
                let delta = if rising { value - prev.last } else { prev.last - value };
                if rising {
                    prev.total_rise = prev.total_rise.saturating_add(delta);
                }
                let index = prev.push(value, recv_ns);
                prev.last = value;
                prev.last_recv_ns = recv_ns;
                prev.count += 1;
                if value > prev.best {
                    prev.best = value;
                }
                (index, Some((delta, interval_ms, rising)))
            }
            None => {
                self.submissions.insert(builder, BuilderSubmissions::new(value, recv_ns));
                (0, None)
            }
        }
    }

    /// What our merged bid for `builder` is compared against, as
    /// (latest, best). The bid sorter honours cancellations, so `latest` is
    /// the real reference and `best` is an upper bound on it.
    pub fn reference_bids(&self, builder: &Address) -> (U256, U256) {
        self.submissions.get(builder).map(|s| (s.last, s.best)).unwrap_or_default()
    }

    /// Builders ranked by their best bid this slot, highest first.
    pub fn top_builders(&self, k: usize) -> Vec<Address> {
        let mut ranked: Vec<(Address, U256)> =
            self.submissions.iter().map(|(addr, s)| (*addr, s.best)).collect();
        ranked.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
        ranked.into_iter().take(k).map(|(addr, _)| addr).collect()
    }
}

/// Ingest-side state for the current slot, owned by the engine thread.
pub struct SlotState {
    pub slot: u64,
    pub parent_hash: B256,
    pub ctx: Arc<SlotContext>,
    /// Shared with the merge streams; `None` until the relay config arrives.
    pub shared: Option<Arc<SharedSlot>>,
    pub blocks: FxHashMap<B256, Arc<PreparedBlock>>,
    /// Sender-recovery cache: incremental submissions share most txs.
    pub recovery_cache: FxHashMap<B256, ethrex_common::Address>,
    /// Decoded txs already seen whole on this connection this slot, keyed by
    /// hash; resolves `MergeableBlockV1` tx-hash references (see
    /// `order::is_tx_hash_ref`). Scope matches the relay's own `sent_txs`
    /// cache: per connection, cleared every slot.
    pub tx_cache: FxHashMap<B256, Arc<DecodedTx>>,
    /// Builder the relay last named as top bid. Advisory: the streams merge
    /// for every candidate builder regardless, since we cannot know which one
    /// will be top when get_header is called.
    pub top_builder: Option<Address>,
    pub stream: StreamStats,
}

/// Per-slot stream counters, logged at slot end.
#[derive(Debug, Default)]
pub struct StreamStats {
    pub offered: u64,
    pub superseded: u64,
    pub refused: u64,
    pub skipped_not_top_k: u64,
}

/// Wall-clock stamps along one base's path from arrival to emission. A zero
/// means that phase did not happen: an inline replay never goes near a worker.
#[derive(Debug, Default, Clone, Copy)]
pub struct Timeline {
    pub recv_ns: u64,
    pub ingest_done_ns: u64,
    pub dispatch_ns: u64,
    pub replay_start_ns: u64,
    pub replay_end_ns: u64,
    /// Engine processed the worker's result.
    pub handled_ns: u64,
    /// Became the live session.
    pub live_ns: u64,
}

/// One submission in a builder's stream, kept so the exact win condition can
/// be evaluated at emission instead of reconstructed from aggregates.
#[derive(Debug, Clone, Copy)]
pub struct Submission {
    pub index: u64,
    pub value: U256,
    pub recv_ns: u64,
}

/// Submissions retained per builder. Bounds the history walk; a builder sends
/// on the order of 150 blocks a slot, so this covers a full slot in practice.
pub const MAX_SUBMISSION_HISTORY: usize = 512;

#[derive(Debug, Clone)]
pub struct BuilderSubmissions {
    pub best: U256,
    pub last: U256,
    pub last_recv_ns: u64,
    pub count: u64,
    /// Total upward bid movement this slot, against `span_ms`, gives the rate.
    pub total_rise: U256,
    pub first_recv_ns: u64,
    /// Recent submissions, oldest first.
    pub history: VecDeque<Submission>,
}

impl BuilderSubmissions {
    fn new(value: U256, recv_ns: u64) -> Self {
        let mut history = VecDeque::with_capacity(MAX_SUBMISSION_HISTORY);
        history.push_back(Submission { index: 0, value, recv_ns });
        Self {
            best: value,
            last: value,
            last_recv_ns: recv_ns,
            count: 1,
            total_rise: U256::ZERO,
            first_recv_ns: recv_ns,
            history,
        }
    }

    pub fn span_ms(&self) -> u64 {
        self.last_recv_ns.saturating_sub(self.first_recv_ns) / 1_000_000
    }

    fn push(&mut self, value: U256, recv_ns: u64) -> u64 {
        let index = self.count;
        if self.history.len() == MAX_SUBMISSION_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Submission { index, value, recv_ns });
        index
    }

    /// How long we had to emit a merged block on base `(base_index,
    /// base_value)` and still beat this builder's own stream, given
    /// `delta` of added proposer value.
    ///
    /// Walks forward from the base to the last submission whose uplift over
    /// the base stays under `delta`; the next one after that is the deadline.
    /// Returns (steps allowed, budget in ms, deadline known). When the whole
    /// retained history stays under `delta` the budget is a lower bound, since
    /// the deadline has not happened yet.
    pub fn budget(&self, base_index: u64, base_value: U256, delta: U256) -> (u64, u64, bool) {
        let Some(base) = self.history.iter().find(|s| s.index == base_index) else {
            return (0, 0, false);
        };
        let mut last_ok = base;
        for sample in self.history.iter().filter(|s| s.index > base_index) {
            if sample.value.saturating_sub(base_value) >= delta {
                return (
                    sample.index - base_index - 1,
                    sample.recv_ns.saturating_sub(base.recv_ns) / 1_000_000,
                    true,
                );
            }
            last_ok = sample;
        }
        (
            last_ok.index - base_index,
            last_ok.recv_ns.saturating_sub(base.recv_ns) / 1_000_000,
            false,
        )
    }
}

impl SlotState {
    pub fn new(msg: &helix_tcp_types::merging::relay_to_builder::SlotStartV1) -> Self {
        let ctx = Arc::new(SlotContext {
            slot: msg.slot,
            parent_hash: msg.parent_hash,
            proposer_fee_recipient: msg.proposer_fee_recipient,
            parent_beacon_block_root: msg.parent_beacon_block_root,
        });
        Self {
            slot: ctx.slot,
            parent_hash: ctx.parent_hash,
            ctx,
            shared: None,
            blocks: FxHashMap::default(),
            recovery_cache: FxHashMap::default(),
            tx_cache: FxHashMap::default(),
            top_builder: None,
            stream: StreamStats::default(),
        }
    }
}
