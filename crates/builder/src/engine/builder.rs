//! One builder's merges under B. A base is diffed against the builder's previous base, orders go
//! on top as effects (most already known from earlier passes and other builders' runs), and each
//! emission runs only the payment, withdrawals and requests before one patch of the trie.

use std::{
    sync::{Arc, OnceLock, RwLock, atomic::Ordering},
    time::Instant,
};

use alloy_primitives::{Address, B256, U256};
use crossbeam_channel::Sender;
use ethrex_blockchain::{payload::calc_gas_limit, vm::StoreVmDatabase};
use ethrex_common::{
    Bloom, H256,
    constants::{DEFAULT_OMMERS_HASH, GAS_PER_BLOB},
    types::{
        AccountInfo, AccountUpdate, BlockHeader, ELASTICITY_MULTIPLIER, Receipt, Transaction,
        Withdrawal, bloom_from_logs, calc_excess_blob_gas, calculate_base_fee_per_gas,
        compute_withdrawals_root,
        requests::{EncodedRequests, compute_requests_hash},
    },
};
use ethrex_crypto::{keccak::keccak_hash, native::NativeCrypto};
use ethrex_levm::{
    db::{CachingDatabase, Database, gen_db::GeneralizedDatabase},
    vm::VMType,
};
use ethrex_storage::Store;
use ethrex_vm::{DynVmDatabase, Evm};
use helix_tcp_types::merging::{
    builder_to_relay::{BuilderInclusion, MergeTraceV1, MergedBlockV1, UnmergedReason, UnmergedTx},
    control::RelayConfigV1,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::info;

use crate::{
    engine::{
        EngineOutput,
        convert::{au256, eaddr, ewithdrawal, h256, payload_v3, requests_to_v4},
        error::{MergeError, SimulationError},
        list_root::ListRoot,
        payment::{self, DistributionConfig, PaymentInputs},
        record::SlotRecorder,
        state::{Coinbase, Effect, Exec, Key, Known, Prior, Val, View, diff, execute, updates},
        streams::{BuilderStream, POLL, StreamJob},
        trie::State,
        types::{EngineConfig, OriginRevenue, PreparedBlock, PreparedOrder, SharedSlot},
    },
    metrics,
    utils::utcnow_ns,
};

pub(crate) const EVM_STACK: usize = 32 << 20;

static POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .stack_size(EVM_STACK)
        .thread_name(|i| format!("b-order-{i}"))
        .build()
        .expect("b order pool")
});

/// The slot-wide part of B: what every builder's runs learn from, and the parent state.
#[derive(Default)]
pub struct Slot {
    known: RwLock<Known>,
    prewarmed: crate::engine::state::Prewarmed,
    parent: OnceLock<Result<Parent, String>>,
}

struct Parent {
    header: BlockHeader,
    /// The parent state, cached for every base of the slot.
    db: Arc<dyn Database>,
    /// The slot's system-call writes, before any tx, as updates and as values.
    system: Vec<AccountUpdate>,
    writes: Arc<FxHashMap<Key, Val>>,
}

impl Parent {
    fn open(
        store: &Store,
        header: &BlockHeader,
        recorder: Option<&Arc<SlotRecorder>>,
    ) -> Result<Self, String> {
        let parent = store
            .get_block_header_by_hash(header.parent_hash)
            .map_err(|e| e.to_string())?
            .ok_or("parent not synced")?;
        let vm_db =
            StoreVmDatabase::new(store.clone(), parent.clone()).map_err(|e| e.to_string())?;
        let mut inner: Arc<dyn Database> = Arc::new(Box::new(vm_db) as DynVmDatabase);
        if let Some(recorder) = recorder {
            let _unused = recorder.parent_header.set(parent.clone());
            let _unused = recorder.store.set(store.clone());
            inner = recorder.wrap(inner);
        }
        let db: Arc<dyn Database> = Arc::new(CachingDatabase::new(inner, true));
        let mut vm = evm(db.clone());
        vm.apply_system_calls(header).map_err(|e| e.to_string())?;
        let system = vm.get_state_transitions().map_err(|e| e.to_string())?;
        let mut writes = FxHashMap::default();
        for update in &system {
            for (slot, value) in &update.added_storage {
                writes.insert(Key::Slot(update.address, *slot), Val::Slot(*value));
            }
        }
        Ok(Parent { header: parent, db, system, writes: Arc::new(writes) })
    }
}

impl Parent {
    fn prefetch(&self, store: &Store, paths: &[(H256, Vec<H256>)]) {
        if let Ok(mut trie) = self.trie(store) {
            let _unused = trie.prefetch(paths);
        }
    }

    /// The parent's state trie.
    fn trie(&self, store: &Store) -> Result<State, String> {
        let root = self.header.state_root;
        let accounts = store.state_trie_db(root).map_err(|e| e.to_string())?;
        let storage = store.clone();
        Ok(State::open(
            accounts,
            root,
            Box::new(move |hashed| {
                storage
                    .storage_trie_db(hashed, root)
                    .map_err(|e| ethrex_trie::TrieError::Verify(e.to_string()))
            }),
        ))
    }
}

/// Learns the effects of orders as they are pooled, off every merge's path.
static PREWARM: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .stack_size(EVM_STACK)
        .thread_name(|i| format!("b-prewarm-{i}"))
        .build()
        .expect("b prewarm pool")
});

static INGEST: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .stack_size(EVM_STACK)
        .thread_name(|i| format!("b-ingest-{i}"))
        .build()
        .expect("b ingest pool")
});

/// Runs newly pooled `orders` from `source` on the parent state, off every merge's path: their
/// effects become known, so merges check them instead of running them, and the trie paths they
/// write are loaded for the emission roots. Kept off the merge pool: this blocks on `known`,
/// which a merge holds across its own parallel runs there.
pub fn learn(
    shared: Arc<SharedSlot>,
    store: Store,
    source: Arc<PreparedBlock>,
    orders: Vec<Arc<PreparedOrder>>,
) {
    let header = Arc::new(header(&source, shared.ctx.parent_beacon_block_root));
    let coinbase = eaddr(source.beneficiary());
    // One task per order and none waits on others: a worker waiting on its own parallel work
    // can pick up another of these tasks, which would ask for `known` again while holding it.
    for order in orders {
        let (shared, store, header) = (shared.clone(), store.clone(), header.clone());
        INGEST.spawn(move || {
            let parent = shared
                .state
                .parent
                .get_or_init(|| Parent::open(&store, &header, shared.ctx.recorder.as_ref()));
            let Ok(parent) = parent.as_ref() else { return };
            let Ok(state) = parent.db.get_account_state(coinbase) else { return };
            let ran = {
                let Ok(known) = shared.state.known.read() else { return };
                let view = View {
                    prior: Arc::new(Prior::new(B256::ZERO, Vec::new(), &mut Known::default())),
                    at: 0,
                    layers: vec![Arc::new(FxHashMap::default())],
                    coinbase: Coinbase {
                        address: coinbase,
                        balance: state.balance,
                        nonce: state.nonce,
                    },
                    codes: known.codes.clone(),
                    system: parent.writes.clone(),
                    store: parent.db.clone(),
                };
                learn_order(&order, view, &header, &known)
            };
            if ran.is_empty() {
                return;
            }
            let paths = paths(ran.iter().flat_map(|effect| &effect.writes).map(|(key, _)| *key));
            if let Ok(mut known) = shared.state.known.write() {
                for effect in ran {
                    known.add(effect);
                }
            }
            parent.prefetch(&store, &paths);
        });
    }
}

pub fn prefetch(shared: Arc<SharedSlot>, store: Store, base: Arc<PreparedBlock>) {
    INGEST.spawn(move || {
        let header = header(&base, shared.ctx.parent_beacon_block_root);
        let parent = shared
            .state
            .parent
            .get_or_init(|| Parent::open(&store, &header, shared.ctx.recorder.as_ref()));
        let Ok(parent) = parent.as_ref() else { return };
        let mut keys: Vec<Key> = Vec::new();
        for tx in base.txs.iter() {
            keys.push(Key::Account(tx.sender));
            if let ethrex_common::types::TxKind::Call(to) = tx.tx.to() {
                keys.push(Key::Account(to));
            }
        }
        let (cold, codes) = {
            let Ok(known) = shared.state.known.read() else { return };
            let writes = base.txs.iter().flat_map(|tx| known.variants(&tx.hash));
            keys.extend(writes.flat_map(|effect| effect.writes.iter().map(|(key, _)| *key)));
            let cold: Vec<Arc<crate::engine::types::DecodedTx>> = base
                .txs
                .iter()
                .filter(|tx| {
                    known.variants(&tx.hash).next().is_none() &&
                        !shared.state.prewarmed.contains_key(&tx.hash)
                })
                .cloned()
                .collect();
            (cold, known.codes.clone())
        };
        if let Ok(state) = parent.db.get_account_state(eaddr(base.beneficiary())) {
            let header = Arc::new(header);
            let coinbase = Coinbase {
                address: eaddr(base.beneficiary()),
                balance: state.balance,
                nonce: state.nonce,
            };
            for tx in cold {
                let (shared, header, codes) = (shared.clone(), header.clone(), codes.clone());
                let (system, db) = (parent.writes.clone(), parent.db.clone());
                PREWARM.spawn(move || {
                    let view = View {
                        prior: Arc::new(Prior::new(B256::ZERO, Vec::new(), &mut Known::default())),
                        at: 0,
                        layers: vec![Arc::new(FxHashMap::default())],
                        coinbase,
                        codes,
                        system,
                        store: db,
                    };
                    if let Some(effect) = execute(view, &tx, &header) {
                        shared.state.prewarmed.insert(tx.hash, Arc::new(effect));
                    }
                });
            }
        }
        parent.prefetch(&store, &paths(keys.into_iter()));
    });
}

fn paths(keys: impl Iterator<Item = Key>) -> Vec<(H256, Vec<H256>)> {
    let mut paths: FxHashMap<H256, Vec<H256>> = FxHashMap::default();
    for key in keys {
        match key {
            Key::Account(address) => {
                paths.entry(H256(keccak_hash(address.as_bytes()))).or_default();
            }
            Key::Slot(address, slot) => paths
                .entry(H256(keccak_hash(address.as_bytes())))
                .or_default()
                .push(H256(keccak_hash(slot.as_bytes()))),
        }
    }
    paths.into_iter().collect()
}

/// The effects `order`'s txs have run in sequence on `view`, for those not already known there.
fn learn_order(
    order: &PreparedOrder,
    mut view: View,
    header: &BlockHeader,
    known: &Known,
) -> Vec<Arc<Effect>> {
    let mut ran = Vec::new();
    for tx in &order.txs {
        let effect = match known.variants(&tx.hash).find(|e| view.holds(e)) {
            Some(effect) => effect.clone(),
            None => match execute(view.clone(), tx, header) {
                Some(effect) => {
                    let effect = Arc::new(effect);
                    ran.push(effect.clone());
                    effect
                }
                None => continue,
            },
        };
        apply(&mut view, &effect);
    }
    ran
}

fn evm(db: Arc<dyn Database>) -> Evm {
    Evm {
        db: GeneralizedDatabase::new(db),
        vm_type: VMType::L1,
        crypto: Arc::new(NativeCrypto),
        stateless_validator: None,
    }
}

/// One builder's B state, kept by its stream across the slot's bases.
#[derive(Default)]
pub struct Builder {
    slot: u64,
    parent: B256,
    streams: FxHashMap<alloy_rpc_types::beacon::BlsPublicKey, Stream>,
    latest: Option<alloy_rpc_types::beacon::BlsPublicKey>,
    carry: Option<(B256, Vec<Applied>)>,
}

type Applied = (Arc<PreparedOrder>, Vec<Option<Arc<Effect>>>);

struct Stream {
    prior: Arc<Prior>,
    trie: Option<(B256, State, Written)>,
}

type Written = FxHashMap<Key, Write>;

type Tree = (State, Written);

#[derive(Clone, PartialEq)]
enum Write {
    Removed,
    Info(AccountInfo),
    Slot(ethrex_common::U256),
}

fn writes(updates: &[AccountUpdate]) -> Result<Written, MergeError> {
    let mut written = Written::default();
    for update in updates {
        if update.removed_storage {
            return Err(MergeError::Internal("storage reset in emission".into()));
        }
        let address = update.address;
        if update.removed {
            written.insert(Key::Account(address), Write::Removed);
            continue;
        }
        if let Some(info) = &update.info {
            written.insert(Key::Account(address), Write::Info(info.clone()));
        }
        for (slot, value) in &update.added_storage {
            written.insert(Key::Slot(address, *slot), Write::Slot(*value));
        }
    }
    Ok(written)
}

impl Builder {
    /// One base: build it, then keep improving it until the builder sends a newer one.
    pub fn merge(
        &mut self,
        id: usize,
        job: &StreamJob,
        stream: &BuilderStream,
        store: &Store,
        out: &Sender<EngineOutput>,
    ) -> Result<U256, MergeError> {
        let shared = &job.shared;
        if shared.ctx.slot < self.slot {
            return Ok(U256::ZERO);
        }
        if shared.ctx.slot != self.slot || shared.ctx.parent_hash != self.parent {
            *self = Builder {
                slot: shared.ctx.slot,
                parent: shared.ctx.parent_hash,
                ..Default::default()
            };
        }
        let warming =
            shared.engine_config.warmup && !self.streams.contains_key(&job.base.builder_pubkey);
        metrics::set_warming(warming);
        let merged = self.merge_base(id, job, stream, warming, store, out);
        if warming {
            stream.try_take();
        }
        merged
    }

    fn merge_base(
        &mut self,
        id: usize,
        job: &StreamJob,
        stream: &BuilderStream,
        warming: bool,
        store: &Store,
        out: &Sender<EngineOutput>,
    ) -> Result<U256, MergeError> {
        let shared = &job.shared;
        let start_ns = utcnow_ns();
        let mut merge = self.base(job, store)?;
        let built_ns = utcnow_ns();
        metrics::stage_latency("b_base", (built_ns - start_ns) / 1000);
        let mut passes = 0u64;
        let mut first_emit_ns = 0;
        let mut seen_version = u64::MAX;
        let mut failed = None;
        loop {
            let version = shared.pool_version.load(Ordering::Acquire);
            let pool_moved = version != seen_version;
            if pool_moved && first_emit_ns > 0 && stream.has_waiting() {
                break;
            }
            if pool_moved {
                seen_version = version;
                passes += 1;
                let (orders, excluded) = {
                    let inner = shared.inner.read().expect("shared slot poisoned");
                    (inner.orders.clone(), inner.excluded.clone())
                };
                let extend_start = Instant::now();
                // The trie patch runs on the pool while this thread extends; the pool never
                // waits on `known`, which this thread holds across the orders' parallel runs.
                match merge.trie.as_mut().err().and_then(Option::take) {
                    Some(trie) => {
                        let (tx, rx) = crossbeam_channel::bounded(1);
                        let (store, slot) = (store.clone(), shared.clone());
                        let spawned = Instant::now();
                        POOL.spawn(move || {
                            let parent = slot.state.parent.get().and_then(|p| p.as_ref().ok());
                            let trie = match parent {
                                Some(parent) => trie.run(&store, parent, spawned),
                                None => Err(MergeError::Internal("no parent".into())),
                            };
                            let _unused = tx.send(trie);
                        });
                        merge.extend(&orders, &excluded, &shared.state.known);
                        let wait = Instant::now();
                        let (trie, leaves, lists, run) = rx
                            .recv()
                            .map_err(|_| MergeError::Internal("trie job lost".into()))??;
                        merge.trie = Ok(trie);
                        merge.leaves = leaves;
                        merge.lists = lists;
                        merge.trie_run = Some(run);
                        merge.trie_wait_us = wait.elapsed().as_micros() as u64;
                    }
                    None => merge.extend(&orders, &excluded, &shared.state.known),
                }
                merge.extend_us = extend_start.elapsed().as_micros() as u64;
                if warming {
                    break;
                }
                if first_emit_ns > 0 && stream.has_waiting() {
                    break;
                }
                let emitted = match merge.emit(
                    shared.ctx.slot,
                    &shared.ctx.proposer_fee_recipient,
                    &shared.relay_config,
                    &shared.engine_config,
                ) {
                    Ok(emitted) => emitted,
                    Err(err) => {
                        failed = Some(err);
                        break;
                    }
                };
                if let Some(msg) = emitted {
                    let now = utcnow_ns();
                    if first_emit_ns == 0 {
                        first_emit_ns = now;
                        let base = &job.base;
                        for (stage, us) in [
                            (
                                "first_ingest",
                                base.ingest_done_ns.saturating_sub(base.recv_ns) / 1000,
                            ),
                            ("first_wait", start_ns.saturating_sub(base.ingest_done_ns) / 1000),
                            ("first_base", (built_ns - start_ns) / 1000),
                            ("first_extend", merge.extend_us),
                            ("first_trie_wait", merge.trie_wait_us),
                            ("first_evm", merge.evm_us),
                            ("first_state_root", merge.state_us),
                            ("first_roots", merge.roots_us),
                            ("first_total", now.saturating_sub(base.recv_ns) / 1000),
                        ] {
                            metrics::stage_latency(stage, us);
                        }
                    }
                    let (own_latest, _) = shared
                        .inner
                        .read()
                        .map(|inner| inner.reference_bids(&job.base.beneficiary()))
                        .unwrap_or_default();
                    let base_age_ms = now.saturating_sub(job.base.recv_ns) / 1_000_000;
                    let won = msg.proposer_value > own_latest;
                    info!(
                        worker = id,
                        engine = "b",
                        base_block_hash = %job.base.block_hash,
                        builder = %job.base.beneficiary(),
                        base_age_ms,
                        pass = passes,
                        won,
                        servable = won &&
                            base_age_ms <= shared.engine_config.max_base_age.as_millis() as u64,
                        ingest_us = job.base.ingest_done_ns.saturating_sub(job.base.recv_ns) / 1000,
                        wait_us = start_ns.saturating_sub(job.base.ingest_done_ns) / 1000,
                        replay_us = (built_ns - start_ns) / 1000,
                        live_us = (now - built_ns) / 1000,
                        base_txs = job.base.txs.len(),
                        run = merge.run,
                        base_warm = merge.warmed,
                        same_stream = merge.same_stream,
                        extend_us = merge.extend_us,
                        ext_orders = merge.extend_stats.orders,
                        ext_txs = merge.extend_stats.txs,
                        ext_presim_ran = merge.extend_stats.presim_ran,
                        ext_apply_ran = merge.extend_stats.apply_ran,
                        ext_filter_us = merge.extend_stats.filter_us,
                        ext_presim_us = merge.extend_stats.presim_us,
                        ext_apply_us = merge.extend_stats.apply_us,
                        ext_known_us = merge.extend_stats.known_us,
                        ext_carried = merge.extend_stats.carried,
                        ext_carry_reran = merge.extend_stats.carry_reran,
                        ext_carry_us = merge.extend_stats.carry_us,
                        trie_wait_us = merge.trie_wait_us,
                        trie = merge.trie_run.map_or("", |run| run.kind),
                        trie_us = merge.trie_run.map_or(0, |run| run.us),
                        trie_reads = merge.trie_run.map_or(0, |run| run.reads),
                        trie_read_us = merge.trie_run.map_or(0, |run| run.read_us),
                        trie_queue_us = merge.trie_run.map_or(0, |run| run.queue_us),
                        trie_leaves_us = merge.trie_run.map_or(0, |run| run.leaves_us),
                        trie_updates_us = merge.trie_run.map_or(0, |run| run.updates_us),
                        trie_apply_us = merge.trie_run.map_or(0, |run| run.apply_us),
                        trie_keys = merge.trie_run.map_or(0, |run| run.keys),
                        evm_us = merge.evm_us,
                        roots_us = merge.roots_us,
                        state_us = merge.state_us,
                        state_keys = merge.state_keys,
                        state_reads = merge.state_reads,
                        state_read_us = merge.state_read_us,
                        "merged block emitted"
                    );
                    let msg =
                        EngineOutput::Merged { generation: job.generation, msg: Box::new(msg) };
                    if out.send(msg).is_err() {
                        break;
                    }
                }
            }
            if stream.has_waiting() {
                break;
            }
            if utcnow_ns().saturating_sub(job.base.recv_ns) >=
                shared.engine_config.max_base_age.as_nanos() as u64
            {
                break;
            }
            if !pool_moved {
                stream.wait_for_change(POLL);
            }
        }
        info!(
            reason = if warming { "warmup" } else { "base_done" },
            engine = "b",
            base_block_hash = %job.base.block_hash,
            builder = %job.base.beneficiary(),
            wait_ms = start_ns.saturating_sub(job.base.ingest_done_ns) / 1_000_000,
            cycle_ms = first_emit_ns.saturating_sub(start_ns) / 1_000_000 * u64::from(first_emit_ns > 0),
            first_emit_age_ms = first_emit_ns.saturating_sub(job.base.recv_ns) / 1_000_000 *
                u64::from(first_emit_ns > 0),
            orders_included = merge.included_order_ids.len(),
            emissions = merge.emissions,
            "merge session stats"
        );
        let trie = match merge.trie {
            Ok(trie) => Some(trie),
            Err(job) => shared
                .state
                .parent
                .get()
                .and_then(|parent| parent.as_ref().ok())
                .zip(job)
                .and_then(|(parent, job)| job.run(store, parent, Instant::now()).ok())
                .map(|(trie, ..)| trie),
        };
        if let Some(stream) = self.streams.get_mut(&job.base.builder_pubkey) {
            stream.trie = trie.map(|(trie, written)| (job.base.block_hash, trie, written));
        }
        self.carry = Some((job.base.block_hash, merge.applied));
        match failed {
            Some(err) => Err(err),
            None => Ok(merge.best_emitted.saturating_sub(job.base.block_value)),
        }
    }

    /// Validates the base and builds it from the builder's previous base.
    fn base(&mut self, job: &StreamJob, store: &Store) -> Result<Merge, MergeError> {
        let base = &job.base;
        let ctx = &job.shared.ctx;
        let relay_config = &job.shared.relay_config;
        let v1 = &base.payload.payload_inner.payload_inner;
        let builder = v1.fee_recipient;
        let builder_safe =
            relay_config.collateral_safe(&builder).ok_or(MergeError::UnknownCollateral(builder))?;
        if base.txs.is_empty() {
            return Err(MergeError::InvalidPayment);
        }
        let distribution_gas_limit = relay_config.distribution_gas_limit;
        let gas_left = v1.gas_limit.saturating_sub(v1.gas_used);
        if gas_left <= distribution_gas_limit {
            return Err(MergeError::InvalidBaseBlock(format!(
                "insufficient gas headroom for distribution: {gas_left}"
            )));
        }
        let chain_config = store.get_chain_config();
        if chain_config.is_amsterdam_activated(v1.timestamp) {
            return Err(MergeError::InvalidBaseBlock(
                "post-Amsterdam blocks are not supported by merging protocol v1".into(),
            ));
        }
        let max_blobs = chain_config
            .get_fork_blob_schedule(v1.timestamp)
            .map(|schedule| schedule.max as u64)
            .unwrap_or_default();
        let blobs: u64 = base.txs.iter().map(|tx| tx.blob_hashes.len() as u64).sum();
        if blobs > max_blobs {
            return Err(MergeError::InvalidBaseBlock(format!(
                "base block uses {blobs} blobs, max {max_blobs}"
            )));
        }

        let header = header(base, ctx.parent_beacon_block_root);
        let parent = job
            .shared
            .state
            .parent
            .get_or_init(|| Parent::open(store, &header, ctx.recorder.as_ref()))
            .as_ref()
            .map_err(|e| MergeError::Internal(e.clone()))?;
        if header.number != parent.header.number + 1 {
            return Err(MergeError::InvalidBaseBlock("block number mismatch".into()));
        }
        if calc_gas_limit(parent.header.gas_limit, v1.gas_limit) != v1.gas_limit {
            return Err(MergeError::InvalidBaseBlock(format!(
                "gas limit {} out of bounds",
                v1.gas_limit
            )));
        }
        let base_fee = calculate_base_fee_per_gas(
            v1.gas_limit,
            parent.header.gas_limit,
            parent.header.gas_used,
            parent.header.base_fee_per_gas.unwrap_or_default(),
            ELASTICITY_MULTIPLIER,
        );
        if base_fee != header.base_fee_per_gas {
            return Err(MergeError::InvalidBaseBlock("base fee mismatch".into()));
        }
        let excess = chain_config
            .get_fork_blob_schedule(v1.timestamp)
            .map(|schedule| {
                calc_excess_blob_gas(&parent.header, schedule, chain_config.fork(v1.timestamp))
            })
            .unwrap_or_default();
        if excess != base.payload.excess_blob_gas {
            return Err(MergeError::InvalidBaseBlock("excess blob gas mismatch".into()));
        }

        let coinbase = eaddr(builder);
        let proposer = eaddr(ctx.proposer_fee_recipient);
        if proposer == coinbase {
            return Err(MergeError::InvalidPayment);
        }
        let exec = Exec {
            txs: base.txs.clone(),
            header,
            store: parent.db.clone(),
            coinbase,
            proposer,
            system: parent.writes.clone(),
        };
        let pubkey = base.builder_pubkey;
        let same_stream = self.latest == Some(pubkey);
        let prior = match self.streams.get(&self.latest.unwrap_or_default()) {
            Some(stream) => stream.prior.clone(),
            None => Arc::new(Prior::new(B256::ZERO, Vec::new(), &mut Known::default())),
        };
        let report = {
            let known = job.shared.state.known.read().expect("known poisoned");
            diff(&prior, &known, &job.shared.state.prewarmed, &exec)
        }
        .ok_or_else(|| MergeError::InvalidBaseBlock("base tx failed".into()))?;
        let gas_used: u64 = report.applied.iter().map(|e| e.outcome.gas_used).sum();
        if gas_used != v1.gas_used {
            return Err(MergeError::InvalidBaseBlock(format!(
                "base block gas mismatch: declared {}, executed {gas_used}",
                v1.gas_used
            )));
        }
        let coinbase_after = report.coinbase.ok_or(MergeError::InvalidPayment)?;
        let carried = match self.carry.take() {
            Some((at, applied)) if at == prior.base => applied,
            _ => Vec::new(),
        };
        let (next, codes) = {
            let mut known = job.shared.state.known.write().expect("known poisoned");
            for effect in report.ran.iter().chain(&report.warmed) {
                known.add(effect.clone());
            }
            let next = Arc::new(Prior::new(base.block_hash, report.applied, &mut known));
            (next, known.codes.clone())
        };
        let view = View {
            prior: next.clone(),
            at: next.effects.len(),
            layers: vec![Arc::new(FxHashMap::default())],
            coinbase: coinbase_after,
            codes,
            system: parent.writes.clone(),
            store: exec.store.clone(),
        };
        // The proposer's balance change across the last tx is the payment proof: it covers a
        // direct transfer and a contract payment alike, and a tx that reverts shows none.
        let before = report.proposer_before.map(|v| v.balance()).unwrap_or_default();
        let after = view.value(&Key::Account(proposer)).map(|v| v.balance()).unwrap_or_default();
        if au256(after.saturating_sub(before)) < base.block_value {
            return Err(MergeError::InvalidPayment);
        }

        let (layer, missing) = match self.streams.get_mut(&pubkey) {
            Some(stream) => match stream.trie.take() {
                Some((at, trie, written)) if at == stream.prior.base => {
                    (Some((trie, written, stream.prior.clone())), "")
                }
                Some(_) => (None, "other_base"),
                None => (None, "no_trie"),
            },
            None => (None, "new_stream"),
        };
        let trie = TrieJob {
            layer,
            missing,
            prior: next.clone(),
            coinbase: coinbase_after,
            txs: base.txs.clone(),
            deposit_contract: chain_config.deposit_contract_address,
        };
        let dirty = match carried.is_empty() {
            true => FxHashSet::default(),
            false => prior
                .keys()
                .chain(next.keys())
                .filter(|key| prior.last(key) != next.last(key))
                .copied()
                .collect(),
        };
        let spent = next.effects.iter().map(|e| e.outcome.gas_spent).sum();
        self.streams.insert(pubkey, Stream { prior: next.clone(), trie: None });
        self.latest = Some(pubkey);
        let run = report.ran.len();
        let warmed = report.warmed.len();
        Ok(Merge {
            base: base.clone(),
            exec,
            view,
            trie: Err(Some(trie)),
            builder_safe,
            gas_soft_limit: v1.gas_limit - distribution_gas_limit,
            distribution_gas_limit,
            chain_id: chain_config.chain_id,
            prague: chain_config.is_prague_activated(v1.timestamp),
            gas_used,
            max_blobs,
            blobs,
            leaves: Vec::new(),
            lists: [ListRoot::default(), ListRoot::default()],
            appended: Vec::new(),
            spent,
            deposit_contract: chain_config.deposit_contract_address,
            appended_blobs: Vec::new(),
            tx_hashes: base.txs.iter().map(|tx| tx.hash).collect(),
            revenues: FxHashMap::default(),
            included_order_ids: Vec::new(),
            applied: Vec::new(),
            carried,
            dirty,
            unmerged: FxHashMap::default(),
            best_emitted: U256::ZERO,
            emissions: 0,
            extend_us: 0,
            extend_stats: ExtendStats::default(),
            trie_run: None,
            evm_us: 0,
            roots_us: 0,
            state_us: 0,
            state_keys: 0,
            state_reads: 0,
            state_read_us: 0,
            trie_wait_us: 0,
            run,
            warmed,
            same_stream,
            trace: MergeTraceV1 { base_block_recv_ns: base.recv_ns, ..Default::default() },
        })
    }
}

/// One tx's entries in the block's tx and receipt tries, encoded once.
struct Leaf {
    tx: alloy_primitives::Bytes,
    receipt: Vec<u8>,
    bloom: Bloom,
    /// The receipt, when it carries deposit-contract logs that requests read.
    deposit: Option<Receipt>,
}

impl Leaf {
    fn new(tx: &Transaction, receipt: Receipt, deposit_contract: ethrex_common::Address) -> Self {
        let bloom = bloom_from_logs(&receipt.logs, &NativeCrypto);
        Leaf {
            tx: tx.encode_canonical_to_vec().into(),
            receipt: receipt.encode_inner_with_precomputed_bloom(bloom),
            bloom,
            deposit: receipt
                .logs
                .iter()
                .any(|log| log.address == deposit_contract)
                .then_some(receipt),
        }
    }

    fn of(
        tx: &crate::engine::types::DecodedTx,
        effect: &Effect,
        cumulative: u64,
        deposit_contract: ethrex_common::Address,
    ) -> Self {
        let outcome = &effect.outcome;
        let (bloom, receipt) = effect.receipt(tx.tx.tx_type(), cumulative);
        let deposit = outcome.logs.iter().any(|log| log.address == deposit_contract).then(|| {
            Receipt::new(tx.tx.tx_type(), outcome.succeeded, cumulative, outcome.logs.clone())
        });
        Leaf { tx: tx.encoded.clone(), receipt, bloom, deposit }
    }
}

#[derive(Clone, Copy)]
struct TrieRun {
    kind: &'static str,
    us: u64,
    reads: u64,
    read_us: u64,
    queue_us: u64,
    leaves_us: u64,
    updates_us: u64,
    apply_us: u64,
    keys: usize,
}

/// The builder's trie for a new base, its trie at the prior patched with the keys that differ
/// else built from the parent, and the base's tx and receipt entries.
struct TrieJob {
    layer: Option<(State, Written, Arc<Prior>)>,
    missing: &'static str,
    prior: Arc<Prior>,
    coinbase: Coinbase,
    txs: Arc<Vec<Arc<crate::engine::types::DecodedTx>>>,
    deposit_contract: ethrex_common::Address,
}

impl TrieJob {
    fn run(
        self,
        store: &Store,
        parent: &Parent,
        spawned: Instant,
    ) -> Result<(Tree, Vec<Leaf>, [ListRoot; 2], TrieRun), MergeError> {
        let start = Instant::now();
        let queue_us = start.duration_since(spawned).as_micros() as u64;
        crate::engine::trie::STORE_READS.with(|reads| reads.set((0, 0)));
        let mut cumulative = 0;
        let leaves = self
            .txs
            .iter()
            .zip(&self.prior.effects)
            .map(|(tx, effect)| {
                cumulative += effect.outcome.gas_spent;
                Leaf::of(tx, effect, cumulative, self.deposit_contract)
            })
            .collect::<Vec<Leaf>>();
        let leaves_us = start.elapsed().as_micros() as u64;
        let had_layer = self.layer.is_some();
        let mut keys = 0;
        let (mut updates_us, mut apply_us) = (0, 0);
        let n = leaves.len();
        let (trie, lists) = POOL.join(
            || {
                let patched = self.layer.and_then(|(mut trie, written, at_base)| {
                    let at = Instant::now();
                    let mut seen = rustc_hash::FxHashSet::default();
                    let changed: Vec<(Key, Option<Val>)> = at_base
                        .keys()
                        .chain(self.prior.keys())
                        .filter(|key| {
                            let account = match key {
                                Key::Account(address) | Key::Slot(address, _) => {
                                    Key::Account(*address)
                                }
                            };
                            !written.contains_key(key) &&
                                written.get(&account) != Some(&Write::Removed) &&
                                seen.insert(**key)
                        })
                        .filter(|key| self.prior.last(key) != at_base.last(key))
                        .map(|key| (*key, self.prior.last(key).cloned()))
                        .collect();
                    keys = changed.len();
                    let mut updates =
                        updates(changed.into_iter(), &*parent.db, self.coinbase).ok()?;
                    if written.contains_key(&Key::Account(self.coinbase.address)) {
                        for update in
                            updates.iter_mut().filter(|u| u.address == self.coinbase.address)
                        {
                            update.info = None;
                        }
                    }
                    updates_us = at.elapsed().as_micros() as u64;
                    let at = Instant::now();
                    trie.apply(&updates).ok()?;
                    apply_us = at.elapsed().as_micros() as u64;
                    Some((trie, written))
                });
                let (trie, kind) = match patched {
                    Some(trie) => (trie, "patched"),
                    None => (
                        (
                            build(store, parent, &self.prior, self.coinbase)
                                .map_err(|e| MergeError::Internal(format!("trie: {e}")))?,
                            Written::default(),
                        ),
                        if had_layer { "patch_failed" } else { self.missing },
                    ),
                };
                Ok::<_, MergeError>((trie, kind))
            },
            || {
                let mut lists = [ListRoot::default(), ListRoot::default()];
                lists[0].root(n, n, |i| &leaves[i].tx[..]);
                lists[1].root(n, n, |i| &leaves[i].receipt[..]);
                lists
            },
        );
        let (trie, kind) = trie?;
        let (reads, read_ns) = crate::engine::trie::STORE_READS.with(|reads| reads.get());
        Ok((trie, leaves, lists, TrieRun {
            kind,
            us: start.elapsed().as_micros() as u64,
            reads,
            read_us: read_ns / 1000,
            queue_us,
            leaves_us,
            updates_us,
            apply_us,
            keys,
        }))
    }
}

/// The trie for `prior`'s base, built from the parent with every key the base wrote.
fn build(
    store: &Store,
    parent: &Parent,
    prior: &Prior,
    coinbase: Coinbase,
) -> Result<State, String> {
    let mut trie = parent.trie(store)?;
    trie.apply(&parent.system).map_err(|e| e.to_string())?;
    let finals = prior.keys().map(|key| (*key, prior.last(key).cloned()));
    let updates = updates(finals, &*parent.db, coinbase).map_err(|e| e.to_string())?;
    trie.apply(&updates).map_err(|e| e.to_string())?;
    Ok(trie)
}

/// The merged block's header before its roots: the base's own fields.
fn header(base: &PreparedBlock, parent_beacon_block_root: B256) -> BlockHeader {
    let v1 = &base.payload.payload_inner.payload_inner;
    let withdrawals: Vec<Withdrawal> =
        base.payload.payload_inner.withdrawals.iter().map(ewithdrawal).collect();
    BlockHeader {
        parent_hash: h256(v1.parent_hash),
        ommers_hash: *DEFAULT_OMMERS_HASH,
        coinbase: eaddr(v1.fee_recipient),
        number: v1.block_number,
        gas_limit: v1.gas_limit,
        timestamp: v1.timestamp,
        extra_data: v1.extra_data.0.clone(),
        prev_randao: h256(v1.prev_randao),
        base_fee_per_gas: Some(v1.base_fee_per_gas.to::<u64>()),
        withdrawals_root: Some(compute_withdrawals_root(&withdrawals, &NativeCrypto)),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(base.payload.excess_blob_gas),
        parent_beacon_block_root: Some(h256(parent_beacon_block_root)),
        ..Default::default()
    }
}

/// One order run on a view: the effect of each tx it kept, what it paid the coinbase, the gas
/// and blobs it took, and the effects B had to run for it.
struct Run {
    effects: Vec<Option<Arc<Effect>>>,
    payment: U256,
    gas: u64,
    blobs: u64,
    view: View,
    ran: Vec<Arc<Effect>>,
}

/// Runs `order` on `view` with A's inclusion rules: a tx that does not fit or fails is dropped
/// when the order allows it, a revert is kept only when allowed. Each tx takes the first effect
/// that holds, from `hints` then `known`, else runs.
fn run_order(
    order: &PreparedOrder,
    mut view: View,
    header: &BlockHeader,
    (gas_left, blobs_left): (u64, u64),
    known: &Known,
    hints: Option<&[Option<Arc<Effect>>]>,
    trusted: bool,
) -> Result<Run, SimulationError> {
    let before = view.coinbase.balance;
    let mut run = Run {
        effects: vec![None; order.txs.len()],
        payment: U256::ZERO,
        gas: 0,
        blobs: 0,
        view: view.clone(),
        ran: Vec::new(),
    };
    for (i, tx) in order.txs.iter().enumerate() {
        if hints.is_some_and(|hints| hints[i].is_none()) {
            continue;
        }
        let (can_drop, can_revert) = (order.can_drop(i), order.can_revert(i));
        let blobs = tx.blob_hashes.len() as u64;
        if tx.gas_limit > gas_left - run.gas || blobs > blobs_left - run.blobs {
            if !can_drop {
                return Err(if tx.gas_limit > gas_left - run.gas {
                    SimulationError::OutOfBlockGas
                } else {
                    SimulationError::OutOfBlockBlobs
                });
            }
            continue;
        }
        let trusted = trusted &&
            hints.is_some_and(|hints| {
                run.effects[..i].iter().zip(&hints[..i]).all(|(ran, hint)| match (ran, hint) {
                    (Some(ran), Some(hint)) => Arc::ptr_eq(ran, hint),
                    (ran, hint) => ran.is_none() && hint.is_none(),
                })
            });
        let hinted = hints.and_then(|hints| hints[i].clone()).filter(|e| trusted || view.holds(e));
        let effect =
            match hinted.or_else(|| known.variants(&tx.hash).find(|e| view.holds(e)).cloned()) {
                Some(effect) => effect,
                None => match execute(view.clone(), tx, header) {
                    Some(effect) => {
                        let effect = Arc::new(effect);
                        run.ran.push(effect.clone());
                        effect
                    }
                    None if can_drop || can_revert => continue,
                    None => return Err(SimulationError::DropNotAllowed(i)),
                },
            };
        if !effect.outcome.succeeded && !can_revert {
            if can_drop {
                continue;
            }
            return Err(SimulationError::RevertNotAllowed(i));
        }
        if run.gas + effect.outcome.gas_used > gas_left {
            if can_drop {
                continue;
            }
            return Err(SimulationError::OutOfBlockGas);
        }
        run.gas += effect.outcome.gas_used;
        run.blobs += blobs;
        apply(&mut view, &effect);
        run.effects[i] = Some(effect);
    }
    run.payment = au256(view.coinbase.balance.saturating_sub(before));
    if run.payment.is_zero() || run.effects.iter().all(Option::is_none) {
        return Err(SimulationError::ZeroBuilderPayment);
    }
    run.view = view;
    Ok(run)
}

/// Puts `effect` on top of `view`.
fn apply(view: &mut View, effect: &Effect) {
    if let Some(layer) = view.layers.last_mut() {
        let layer = Arc::make_mut(layer);
        for (key, val) in &effect.writes {
            layer.insert(*key, Some(val.clone()));
        }
    }
    if !effect.codes.is_empty() {
        let codes = Arc::make_mut(&mut view.codes);
        for code in &effect.codes {
            codes.insert(code.hash, code.clone());
        }
    }
    view.coinbase = view.coinbase.after(effect);
}

#[derive(Default, Clone, Copy)]
struct ExtendStats {
    orders: usize,
    txs: usize,
    presim_ran: usize,
    apply_ran: usize,
    filter_us: u64,
    presim_us: u64,
    apply_us: u64,
    known_us: u64,
    carried: usize,
    carry_reran: usize,
    carry_us: u64,
}

/// One base B built and the orders put on it so far.
struct Merge {
    base: Arc<PreparedBlock>,
    exec: Exec,
    /// The base and every applied order: the orders' writes are the view's one layer.
    view: View,
    trie: Result<Tree, Option<TrieJob>>,
    builder_safe: Address,
    /// The last extend's and emission's stage timings, in µs.
    extend_us: u64,
    extend_stats: ExtendStats,
    trie_run: Option<TrieRun>,
    evm_us: u64,
    roots_us: u64,
    state_us: u64,
    state_keys: usize,
    state_reads: u64,
    state_read_us: u64,
    trie_wait_us: u64,
    gas_soft_limit: u64,
    distribution_gas_limit: u64,
    chain_id: u64,
    prague: bool,
    gas_used: u64,
    max_blobs: u64,
    blobs: u64,
    /// The base's tx and receipt entries, then each appended tx's.
    leaves: Vec<Leaf>,
    lists: [ListRoot; 2],
    appended: Vec<Leaf>,
    /// Receipt gas through the last appended tx.
    spent: u64,
    deposit_contract: ethrex_common::Address,
    appended_blobs: Vec<B256>,
    tx_hashes: FxHashSet<B256>,
    revenues: FxHashMap<Address, OriginRevenue>,
    included_order_ids: Vec<B256>,
    applied: Vec<Applied>,
    carried: Vec<Applied>,
    dirty: FxHashSet<Key>,
    /// Why each order left out was, with its txs.
    unmerged: FxHashMap<B256, (UnmergedReason, Vec<B256>)>,
    best_emitted: U256,
    emissions: u64,
    /// Base txs B ran.
    run: usize,
    warmed: usize,
    same_stream: bool,
    trace: MergeTraceV1,
}

impl Merge {
    fn gas_left(&self) -> u64 {
        self.gas_soft_limit.saturating_sub(self.gas_used)
    }

    fn blobs_left(&self) -> u64 {
        self.max_blobs.saturating_sub(self.blobs)
    }

    fn unmerged(&mut self, order: &PreparedOrder, reason: UnmergedReason) {
        self.unmerged
            .insert(order.order_id, (reason, order.txs.iter().map(|tx| tx.hash).collect()));
    }

    /// Runs every new order on the current state in parallel, then applies them best payment
    /// first, each re-checked on the state the earlier ones left.
    fn extend(
        &mut self,
        orders: &[Arc<PreparedOrder>],
        excluded: &FxHashSet<B256>,
        known: &RwLock<Known>,
    ) {
        self.trace.sim_start_ns = utcnow_ns();
        let start = Instant::now();
        let carried = std::mem::take(&mut self.carried);
        let (carried_n, mut carry_reran) = (carried.len(), 0);
        let mut ran: Vec<Arc<Effect>> = Vec::new();
        for (order, effects) in carried {
            let writes = |effects: &[Option<Arc<Effect>>], dirty: &mut FxHashSet<Key>| {
                dirty
                    .extend(effects.iter().flatten().flat_map(|e| e.writes.iter().map(|(k, _)| *k)))
            };
            if excluded.contains(&order.order_hash) ||
                order.source_block_hash == self.base.block_hash ||
                order.fits(&self.tx_hashes, self.gas_left(), self.blobs_left()).is_err()
            {
                writes(&effects, &mut self.dirty);
                continue;
            }
            let clean = effects.iter().flatten().all(|e| {
                self.view.coinbase.admits(e) && e.reads.iter().all(|(k, _)| !self.dirty.contains(k))
            });
            carry_reran += usize::from(!clean);
            let mut view = self.view.clone();
            view.layers.push(Arc::new(FxHashMap::default()));
            let space = (self.gas_left(), self.blobs_left());
            let run = {
                let known = known.read().expect("known poisoned");
                run_order(&order, view, &self.exec.header, space, &known, Some(&effects), clean)
            };
            match run {
                Ok(run) => {
                    let same = run.effects.iter().zip(&effects).all(|(a, b)| match (a, b) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    });
                    if !same {
                        writes(&effects, &mut self.dirty);
                        writes(&run.effects, &mut self.dirty);
                    }
                    ran.extend(run.ran.iter().cloned());
                    self.commit(&order, run);
                }
                Err(err) => {
                    writes(&effects, &mut self.dirty);
                    self.unmerged(&order, err.unmerged_reason());
                }
            }
        }
        let carry_us = start.elapsed().as_micros() as u64;
        let applied: FxHashSet<B256> = self.included_order_ids.iter().copied().collect();
        let mut candidates = Vec::new();
        for order in orders {
            if excluded.contains(&order.order_hash) {
                self.unmerged(order, UnmergedReason::Replaced);
                continue;
            }
            if applied.contains(&order.order_id) || order.source_block_hash == self.base.block_hash
            {
                continue;
            }
            match order.fits(&self.tx_hashes, self.gas_left(), self.blobs_left()) {
                Ok(()) => candidates.push(order.clone()),
                Err(err) => self.unmerged(order, err.unmerged_reason()),
            }
        }
        let space = (self.gas_left(), self.blobs_left());
        let presim_start = Instant::now();
        let filter_us = (presim_start - start).as_micros() as u64;
        let mut runs: Vec<(usize, Run)> = {
            let known = known.read().expect("known poisoned");
            let (view, header) = (&self.view, &self.exec.header);
            POOL.install(|| {
                use rayon::prelude::*;
                candidates
                    .par_iter()
                    .enumerate()
                    .map(|(ix, order)| {
                        let mut view = view.clone();
                        view.layers.push(Arc::new(FxHashMap::default()));
                        (ix, run_order(order, view, header, space, &known, None, false))
                    })
                    .collect::<Vec<_>>()
            })
        }
        .into_iter()
        .filter_map(|(ix, run)| match run {
            Ok(run) => Some((ix, run)),
            Err(err) => {
                self.unmerged(&candidates[ix], err.unmerged_reason());
                None
            }
        })
        .collect();
        runs.sort_unstable_by_key(|(_, run)| std::cmp::Reverse(run.payment));
        let carry_ran = ran.len();
        ran.extend(runs.iter().flat_map(|(_, run)| run.ran.clone()));
        let presim_ran = ran.len() - carry_ran;
        let presim_us = presim_start.elapsed().as_micros() as u64;
        let apply_start = Instant::now();

        for (ix, sim) in runs {
            let order = &candidates[ix];
            if order.fits(&self.tx_hashes, self.gas_left(), self.blobs_left()).is_err() {
                self.unmerged(order, UnmergedReason::OutOfSpace);
                continue;
            }
            let mut view = self.view.clone();
            view.layers.push(Arc::new(FxHashMap::default()));
            let space = (self.gas_left(), self.blobs_left());
            let run = {
                let known = known.read().expect("known poisoned");
                run_order(order, view, &self.exec.header, space, &known, Some(&sim.effects), false)
            };
            let run = match run {
                Ok(run) => run,
                Err(err) => {
                    self.unmerged(order, err.unmerged_reason());
                    continue;
                }
            };
            ran.extend(run.ran.iter().cloned());
            self.commit(order, run);
        }
        let apply_us = apply_start.elapsed().as_micros() as u64;
        let apply_ran = ran.len() - presim_ran - carry_ran;
        let known_start = Instant::now();
        if !ran.is_empty() {
            let mut known = known.write().expect("known poisoned");
            for effect in ran {
                known.add(effect);
            }
        }
        self.extend_stats = ExtendStats {
            orders: candidates.len(),
            txs: candidates.iter().map(|order| order.txs.len()).sum(),
            presim_ran,
            apply_ran,
            filter_us,
            presim_us,
            apply_us,
            known_us: known_start.elapsed().as_micros() as u64,
            carried: carried_n,
            carry_reran,
            carry_us,
        };
        self.trace.sim_end_ns = utcnow_ns();
        metrics::stage_latency("b_extend", start.elapsed().as_micros() as u64);
    }

    /// Keeps `order` as `run` applied it: its writes fold into the orders' layer.
    fn commit(&mut self, order: &Arc<PreparedOrder>, mut run: Run) {
        self.applied.push((order.clone(), run.effects.clone()));
        self.view.layers.clear();
        let top = run.view.layers.pop().unwrap_or_default();
        if let Some(layer) = run.view.layers.last_mut() {
            Arc::make_mut(layer).extend(top.iter().map(|(k, v)| (*k, v.clone())));
        }
        self.view = run.view;
        self.gas_used += run.gas;
        self.blobs += run.blobs;
        let mut hashes = Vec::new();
        for (tx, effect) in order.txs.iter().zip(run.effects) {
            let Some(effect) = effect else { continue };
            hashes.push(tx.hash);
            self.tx_hashes.insert(tx.hash);
            self.appended_blobs.extend(tx.blob_hashes.iter().copied());
            self.spent += effect.outcome.gas_spent;
            self.appended.push(Leaf::of(tx, &effect, self.spent, self.deposit_contract));
        }
        self.unmerged.remove(&order.order_id);
        self.included_order_ids.push(order.order_id);
        let entry = self.revenues.entry(order.origin).or_insert_with(|| OriginRevenue {
            revenue: U256::ZERO,
            txs: Vec::new(),
            pubkey: order.builder_pubkey,
        });
        entry.revenue += run.payment;
        entry.txs.extend(hashes);
    }

    /// The merged block with the distribution tx paying out every order's revenue, when it beats
    /// the last emission.
    fn emit(
        &mut self,
        slot: u64,
        proposer: &Address,
        relay_config: &RelayConfigV1,
        engine_config: &EngineConfig,
    ) -> Result<Option<MergedBlockV1>, MergeError> {
        let start = Instant::now();
        let internal = |e: &dyn std::fmt::Display| MergeError::Internal(e.to_string());
        let total_revenue: U256 = self.revenues.values().map(|v| v.revenue).sum();
        let base_fee = self.exec.header.base_fee_per_gas.unwrap_or_default();
        let payment_cost =
            U256::from(base_fee).saturating_mul(U256::from(self.distribution_gas_limit));
        if total_revenue <= payment_cost {
            return Ok(None);
        }
        let distribution = DistributionConfig::from_relay_config(relay_config);
        let revenues = payment::prepare_revenues(
            &distribution,
            &self.revenues,
            payment_cost,
            *proposer,
            relay_config.relay_fee_recipient,
            self.base.beneficiary(),
        );
        let proposer_added = revenues.get(proposer).cloned().unwrap_or_default();
        let proposer_value = self.base.block_value + proposer_added;
        let builder_revenue =
            total_revenue.saturating_sub(revenues.values().sum()).saturating_sub(payment_cost);
        if builder_revenue.is_zero() ||
            proposer_value <= self.best_emitted + engine_config.min_value_increase_wei
        {
            return Ok(None);
        }

        let view = &self.view;
        let safe = eaddr(self.builder_safe);
        let signer = eaddr(engine_config.relay_signer.address());
        let account = |address| view.value(&Key::Account(address));
        let safe_nonce = match view.value(&Key::Slot(safe, H256::from_low_u64_be(5))) {
            Some(Val::Slot(nonce)) => nonce.as_u64(),
            _ => 0,
        };
        let signer_nonce = match account(signer) {
            Some(Val::Account { nonce, .. }) => nonce,
            _ => 0,
        };
        let inputs = PaymentInputs {
            safe: self.builder_safe,
            safe_balance: au256(account(safe).map(|v| v.balance()).unwrap_or_default()),
            safe_nonce,
            signer_nonce,
            chain_id: self.chain_id,
            gas_limit: self.distribution_gas_limit,
            base_fee_per_gas: base_fee as u128,
            multisend_contract: relay_config.multisend_contract,
        };
        let encoded = payment::build_payment_tx(&engine_config.relay_signer, &inputs, &revenues)?;
        let payment_tx = Transaction::decode_canonical(&encoded).map_err(|e| internal(&e))?;
        let payment_sender = payment_tx.sender(&NativeCrypto).map_err(|e| internal(&e))?;

        // Only the distribution tx, requests and withdrawals still run, on a fresh EVM over the
        // merged state.
        let evm_start = Instant::now();
        let mut vm = evm(Arc::new(view.clone()));
        let mut cumulative = self.spent;
        let (receipt, ran) = vm
            .execute_tx(&payment_tx, &self.exec.header, &mut cumulative, payment_sender)
            .map_err(|e| internal(&e))?;
        if !receipt.succeeded {
            return Err(MergeError::RevenueAllocationReverted);
        }
        let payment = Leaf::new(&payment_tx, receipt, self.deposit_contract);
        let leaves: Vec<&Leaf> =
            self.leaves.iter().chain(&self.appended).chain(std::iter::once(&payment)).collect();
        let requests: Vec<EncodedRequests> = if self.prague {
            let deposits: Vec<Receipt> =
                leaves.iter().filter_map(|leaf| leaf.deposit.clone()).collect();
            vm.extract_requests(&deposits, &self.exec.header)
                .map_err(|e| internal(&e))?
                .iter()
                .map(|r| r.encode())
                .collect()
        } else {
            Vec::new()
        };
        let withdrawals: Vec<Withdrawal> =
            self.base.payload.payload_inner.withdrawals.iter().map(ewithdrawal).collect();
        vm.process_withdrawals(&withdrawals).map_err(|e| internal(&e))?;
        let emission = vm.get_state_transitions().map_err(|e| internal(&e))?;

        let orders =
            self.view.layers.first().map(|layer| layer.as_ref().clone()).unwrap_or_default();
        let orders = updates(orders.into_iter(), &*self.exec.store, self.view.coinbase)
            .map_err(|e| internal(&e))?;
        let target = writes(&merged(orders.into_iter().chain(emission)))?;
        self.evm_us = evm_start.elapsed().as_micros() as u64;
        let roots_start = Instant::now();
        let Ok((_, written)) = &self.trie else { return Ok(None) };
        let base = View { layers: Vec::new(), ..self.view.clone() };
        let mut changed: FxHashMap<ethrex_common::Address, AccountUpdate> = FxHashMap::default();
        let resets = written.keys().filter(|key| !target.contains_key(key)).map(|key| {
            let write = match base.value(key) {
                Some(Val::Account { exists: false, .. }) => Write::Removed,
                Some(Val::Account { balance, nonce, code_hash, .. }) => {
                    Write::Info(AccountInfo { code_hash, balance, nonce })
                }
                Some(Val::Slot(value)) => Write::Slot(value),
                None => return Err(internal(&"base value missing")),
            };
            Ok((*key, write))
        });
        let sets = target
            .iter()
            .filter(|(key, write)| written.get(key) != Some(*write))
            .map(|(key, write)| Ok((*key, write.clone())));
        for set in sets.chain(resets) {
            let (key, write) = set?;
            let address = match key {
                Key::Account(address) | Key::Slot(address, _) => address,
            };
            let update = changed.entry(address).or_insert_with(|| AccountUpdate::new(address));
            match (key, write) {
                (_, Write::Removed) => update.removed = true,
                (_, Write::Info(info)) => update.info = Some(info),
                (Key::Slot(_, slot), Write::Slot(value)) => {
                    update.added_storage.insert(slot, value);
                }
                (Key::Account(_), Write::Slot(_)) => return Err(internal(&"slot write on account")),
            }
        }
        let revived = written.iter().filter_map(|(key, write)| match (key, write) {
            (Key::Account(address), Write::Removed) if target.get(key) != Some(&Write::Removed) => {
                Some(*address)
            }
            _ => None,
        });
        for address in revived {
            let slots = self.view.prior.keys().filter(|key| {
                matches!(key, Key::Slot(a, _) if *a == address) && !target.contains_key(key)
            });
            for key in slots {
                let (Key::Slot(_, slot), Some(Val::Slot(value))) = (key, base.value(key)) else {
                    continue;
                };
                changed
                    .entry(address)
                    .or_insert_with(|| AccountUpdate::new(address))
                    .added_storage
                    .insert(*slot, value);
            }
        }
        let changed: Vec<AccountUpdate> = changed.into_values().collect();
        self.state_keys = changed.iter().map(|u| 1 + u.added_storage.len()).sum();
        let [txs, receipts] = &mut self.lists;
        let Ok((trie, _)) = &mut self.trie else { return Ok(None) };
        let (state_root, (transactions_root, (receipts_root, logs_bloom))) = POOL.join(
            || {
                let start = Instant::now();
                crate::engine::trie::STORE_READS.with(|reads| reads.set((0, 0)));
                let root = trie.apply(&changed);
                let reads = crate::engine::trie::STORE_READS.with(|reads| reads.get());
                (root, start.elapsed().as_micros() as u64, reads)
            },
            || {
                let bloom = leaves.iter().fold(Bloom::zero(), |bloom, leaf| bloom | leaf.bloom);
                let n = leaves.len();
                POOL.join(
                    || txs.root(n, n - 1, |i| &leaves[i].tx[..]),
                    || (receipts.root(n, n - 1, |i| &leaves[i].receipt[..]), bloom),
                )
            },
        );
        let (state_root, state_us, (state_reads, state_read_ns)) = state_root;
        self.state_reads = state_reads;
        self.state_read_us = state_read_ns / 1000;
        let state_root = match state_root {
            Ok(root) => root,
            Err(e) => {
                self.trie = Err(None);
                return Err(internal(&e));
            }
        };
        if let Ok((_, written)) = &mut self.trie {
            *written = target;
        }
        self.state_us = state_us;
        self.roots_us = roots_start.elapsed().as_micros() as u64;
        let mut header = self.exec.header.clone();
        header.state_root = state_root;
        header.transactions_root = transactions_root;
        header.receipts_root = receipts_root;
        header.logs_bloom = logs_bloom;
        header.gas_used = self.gas_used + ran.gas_used;
        header.blob_gas_used = Some((self.blobs) * u64::from(GAS_PER_BLOB));
        if self.prague {
            header.requests_hash = Some(compute_requests_hash(&requests));
        }
        let transactions = leaves.iter().map(|leaf| leaf.tx.clone()).collect();
        let execution_payload = payload_v3(&header, transactions, &withdrawals);
        self.trace.finalize_ns = utcnow_ns();
        metrics::stage_latency("b_emit", start.elapsed().as_micros() as u64);

        let builder_inclusions = self
            .revenues
            .iter()
            .map(|(origin, revenue)| BuilderInclusion {
                builder_pubkey: revenue.pubkey,
                origin_coinbase: *origin,
                contribution: revenue.revenue,
                revenue: revenues.get(origin).cloned().unwrap_or_default(),
                txs: revenue.txs.clone(),
            })
            .collect();
        let relay_revenue =
            revenues.get(&relay_config.relay_fee_recipient).cloned().unwrap_or_default();
        self.best_emitted = proposer_value;
        self.emissions += 1;
        Ok(Some(MergedBlockV1 {
            slot,
            response_id: 0,
            base_block_hash: self.base.block_hash,
            base_builder_pubkey: self.base.builder_pubkey,
            execution_payload,
            execution_requests: requests_to_v4(&requests).map_err(|e| internal(&e))?,
            appended_blobs: self.appended_blobs.clone(),
            proposer_value,
            base_builder_revenue: builder_revenue,
            relay_revenue,
            builder_inclusions,
            included_order_ids: self.included_order_ids.clone(),
            trace: self.trace,
            unmerged_txs: self
                .unmerged
                .values()
                .flat_map(|(reason, txs)| {
                    txs.iter().map(|&tx_hash| UnmergedTx { tx_hash, reason: *reason })
                })
                .collect(),
        }))
    }
}

/// One update per account, later updates merged over earlier ones.
fn merged(updates: impl IntoIterator<Item = AccountUpdate>) -> Vec<AccountUpdate> {
    let mut out: Vec<AccountUpdate> = Vec::new();
    let mut at: FxHashMap<ethrex_common::Address, usize> = FxHashMap::default();
    for update in updates {
        match at.get(&update.address) {
            Some(&ix) => out[ix].merge(update),
            None => {
                at.insert(update.address, out.len());
                out.push(update);
            }
        }
    }
    out
}
