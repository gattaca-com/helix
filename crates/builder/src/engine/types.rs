use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::beacon::BlsPublicKey;
use alloy_signer_local::PrivateKeySigner;
use helix_tcp_types::merging::control::RelayConfigV1;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::engine::session::{MergeSession, ReplayCheckpoint};

/// Sessions kept alive after a base switch, resumed instantly when the
/// relay's top bid flips back instead of re-replaying the base block.
pub const MAX_PARKED_SESSIONS: usize = 4;

/// Per-beneficiary replay checkpoints kept alive at once; a defensive cap —
/// distinct beneficiaries this slot are bounded in practice by
/// `builder_collaterals` (<= `MAX_BUILDER_COLLATERALS`).
pub const MAX_REPLAY_CHECKPOINTS: usize = 32;

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
    /// Optional core pin for the engine worker thread.
    pub core: Option<usize>,
    /// Speculative replay workers; 0 disables speculation.
    pub speculation_workers: usize,
    pub speculation_queue_capacity: usize,
    pub max_prebuilt_per_builder: usize,
    /// One core per speculative replay worker; empty leaves them unpinned.
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

/// All merging state for the current slot.
pub struct SlotState {
    pub slot: u64,
    pub parent_hash: B256,
    pub proposer_fee_recipient: Address,
    /// The three fields above plus the beacon root, as the replay workers take them.
    pub ctx: Arc<SlotContext>,
    /// Relay config snapshot taken at slot start.
    pub relay_config: Option<Arc<RelayConfigV1>>,
    pub blocks: FxHashMap<B256, Arc<PreparedBlock>>,
    pub orders: Vec<PreparedOrder>,
    /// order_id -> index into `orders` (dedup; attribution goes to the
    /// highest-value source block).
    pub order_ids: FxHashMap<B256, usize>,
    /// Sender-recovery cache: incremental submissions share most txs.
    pub recovery_cache: FxHashMap<B256, ethrex_common::Address>,
    /// Decoded txs already seen whole on this connection this slot, keyed by
    /// hash; resolves `MergeableBlockV1` tx-hash references (see
    /// `order::is_tx_hash_ref`). Scope matches the relay's own `sent_txs`
    /// cache: per connection, cleared every slot.
    pub tx_cache: FxHashMap<B256, Arc<DecodedTx>>,
    pub session: Option<MergeSession>,
    /// Sessions for previously activated bases, newest last; capped at
    /// [`MAX_PARKED_SESSIONS`].
    pub parked: Vec<MergeSession>,
    /// Activation that arrived before its block finished ingest.
    pub pending_activation: Option<(B256, u64)>,
    /// Post-replay snapshot of the last base built per beneficiary, reused by
    /// `MergeSession::activate` when a later resubmission extends it — see
    /// `ReplayCheckpoint`. Capped at [`MAX_REPLAY_CHECKPOINTS`].
    pub replay_checkpoints: FxHashMap<Address, ReplayCheckpoint>,
    /// Activations that reused a checkpoint vs. replayed the base from
    /// scratch, this slot.
    pub checkpoint_hits: usize,
    pub checkpoint_misses: usize,
    pub excluded: FxHashSet<B256>,
    pub latest_only: FxHashMap<BlsPublicKey, FxHashSet<B256>>,
    /// Sessions replayed speculatively on arrival, keyed by base block hash.
    pub prebuilt: FxHashMap<B256, MergeSession>,
    /// Prebuilt hashes per base builder, oldest first; caps retention.
    pub prebuilt_by_builder: FxHashMap<Address, Vec<B256>>,
    /// Blocks dispatched to a replay worker and not yet answered.
    pub speculating: FxHashSet<B256>,
    pub spec: SpecStats,
}

/// Speculation counters for one slot, logged at slot end.
#[derive(Debug, Default)]
pub struct SpecStats {
    pub dispatched: u64,
    pub queue_full: u64,
    pub completed: u64,
    pub failed: u64,
    pub evicted: u64,
    /// Activations served from `prebuilt` with no replay on the engine thread.
    pub hits: u64,
    /// Activations that fell through to an engine-thread replay.
    pub misses: u64,
}

impl SlotState {
    pub fn update_latest_only(&mut self, pubkey: BlsPublicKey, current: FxHashSet<B256>) {
        if let Some(previous) = self.latest_only.get(&pubkey) {
            for dropped in previous.difference(&current) {
                self.excluded.insert(*dropped);
            }
        }
        self.latest_only.insert(pubkey, current);
    }

    pub fn is_excluded(&self, order_hash: &B256) -> bool {
        self.excluded.contains(order_hash)
    }

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
            proposer_fee_recipient: ctx.proposer_fee_recipient,
            ctx,
            relay_config: None,
            blocks: FxHashMap::default(),
            orders: Vec::new(),
            order_ids: FxHashMap::default(),
            recovery_cache: FxHashMap::default(),
            tx_cache: FxHashMap::default(),
            session: None,
            parked: Vec::new(),
            pending_activation: None,
            replay_checkpoints: FxHashMap::default(),
            checkpoint_hits: 0,
            checkpoint_misses: 0,
            excluded: FxHashSet::default(),
            latest_only: FxHashMap::default(),
            prebuilt: FxHashMap::default(),
            prebuilt_by_builder: FxHashMap::default(),
            speculating: FxHashSet::default(),
            spec: SpecStats::default(),
        }
    }

    /// Stores a speculatively replayed session, evicting the builder's oldest
    /// beyond `max_per_builder`. Returns the number evicted.
    pub fn insert_prebuilt(
        &mut self,
        beneficiary: Address,
        session: MergeSession,
        max_per_builder: usize,
    ) -> usize {
        let block_hash = session.base_block_hash;
        if self.prebuilt.insert(block_hash, session).is_none() {
            self.prebuilt_by_builder.entry(beneficiary).or_default().push(block_hash);
        }
        let mut evicted = 0;
        if let Some(hashes) = self.prebuilt_by_builder.get_mut(&beneficiary) {
            while hashes.len() > max_per_builder {
                let old = hashes.remove(0);
                if let Some(session) = self.prebuilt.remove(&old) {
                    session.log_stats("prebuilt_eviction");
                    evicted += 1;
                }
            }
        }
        evicted
    }

    /// Takes the prebuilt session for `block_hash`, if one was warmed.
    pub fn take_prebuilt(&mut self, block_hash: &B256) -> Option<MergeSession> {
        let session = self.prebuilt.remove(block_hash)?;
        for hashes in self.prebuilt_by_builder.values_mut() {
            hashes.retain(|hash| hash != block_hash);
        }
        Some(session)
    }
}
