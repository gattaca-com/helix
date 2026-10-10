//! Per-base-block merge session: validates and replays the activated base
//! block onto parent state, greedily appends profitable orders, and emits
//! improved `MergedBlockV1`s with the Safe multiSend distribution appended.
//! Port of the simulator's `merge_block` / `BlockBuilder` /
//! `append_greedily_until_gas_limit` (`crates/simulator/src/block_merging/mod.rs`)
//! onto ethrex's `PayloadBuildContext`.

use std::{collections::hash_map::Entry, sync::Arc, time::Instant};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::beacon::BlsPublicKey;
use ethrex_blockchain::{
    Blockchain,
    payload::{BuildPayloadArgs, HeadTransaction, PayloadBuildContext, create_payload},
};
use ethrex_common::{
    Bloom,
    types::{ELASTICITY_MULTIPLIER, bloom_from_logs, calculate_base_fee_per_gas},
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_rlp::encode::RLPEncode;
use ethrex_storage::Store;
use ethrex_trie::Trie;
use flux_profiler::timed;
use helix_tcp_types::merging::{
    builder_to_relay::{BuilderInclusion, MergeTraceV1, MergedBlockV1, UnmergedReason, UnmergedTx},
    control::RelayConfigV1,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, info};

const LAYER_FLUSH_TXS: usize = 8;

/// Order presims run here: the node's block import warms on the global pool,
/// and a presim queued behind it stalls for the whole import.
static PRESIM_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(16)
        .thread_name(|i| format!("presim-{i}"))
        .build()
        .expect("presim pool")
});

/// Kept off the global pool so speculative warming never delays order presims.
static WARM_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .thread_name(|i| format!("warm-{i}"))
        .build()
        .expect("warm pool")
});

use crate::{
    engine::{
        convert::{au256, block_to_payload_v3, eaddr, ewithdrawal, h256, requests_to_v4},
        error::{MergeError, SimulationError},
        payment::{self, DistributionConfig, PaymentInputs},
        reuse,
        simulate::{self, balance_of},
        state_layer::{PendingLayer, StateLayer, UpdateChunk},
        types::{
            EngineConfig, OriginRevenue, PreparedBlock, PreparedOrder, SimulatedOrder, SlotContext,
            Timeline,
        },
    },
    metrics,
    utils::utcnow_ns,
};

/// Upper bound on the priority fee an order can pay, used to size the value
/// lost to each screening outcome. Real payment is lower; this is a ceiling.
fn order_headroom(order: &PreparedOrder, base_fee: Option<u64>) -> U256 {
    let base_fee = base_fee.unwrap_or_default();
    order.txs.iter().fold(U256::ZERO, |acc, tx| {
        let tip = au256(tx.tx.effective_gas_tip(Some(base_fee)).unwrap_or_default());
        acc.saturating_add(tip.saturating_mul(U256::from(tx.gas_limit)))
    })
}

fn sim_error_label(err: &SimulationError) -> &'static str {
    match err {
        SimulationError::ZeroBuilderPayment => "zero_payment",
        SimulationError::OutOfBlockGas => "out_of_gas",
        SimulationError::OutOfBlockBlobs => "out_of_blobs",
        SimulationError::DuplicateTransaction => "duplicate",
        SimulationError::RevertNotAllowed(_) => "revert_not_allowed",
        SimulationError::DropNotAllowed(_) => "drop_not_allowed",
        SimulationError::Execution(_) => "execution_error",
    }
}

struct OrderOutcome {
    label: &'static str,
    reason: Option<UnmergedReason>,
    headroom: U256,
    txs: Vec<B256>,
}

/// Result of an emission attempt.
pub enum EmitOutcome {
    Emitted(Box<MergedBlockV1>),
    /// No merged revenue, no improvement over the last emission, or the
    /// improvement doesn't cover the distribution cost; nothing to retry.
    NotImproved,
}

/// Aggregate screening/emission counters for one merge session, logged when
/// the session is finally discarded.
#[derive(Debug, Default)]
pub struct MergeStats {
    pub candidates_screened: u64,
    pub presim_zero_payment: u64,
    pub presim_out_of_gas: u64,
    pub presim_out_of_blobs: u64,
    pub presim_duplicate: u64,
    pub presim_revert_not_allowed: u64,
    pub presim_drop_not_allowed: u64,
    pub presim_execution_error: u64,
    pub orders_applied: u64,
    pub apply_rollbacks: u64,
    pub emissions: u64,
    pub emit_not_improved: u64,
    pub emit_no_revenue: u64,
    pub orders_excluded_skipped: u64,
}

impl MergeStats {
    fn count_sim_error(&mut self, err: &SimulationError) {
        match err {
            SimulationError::ZeroBuilderPayment => self.presim_zero_payment += 1,
            SimulationError::OutOfBlockGas => self.presim_out_of_gas += 1,
            SimulationError::OutOfBlockBlobs => self.presim_out_of_blobs += 1,
            SimulationError::DuplicateTransaction => self.presim_duplicate += 1,
            SimulationError::RevertNotAllowed(_) => self.presim_revert_not_allowed += 1,
            SimulationError::DropNotAllowed(_) => self.presim_drop_not_allowed += 1,
            SimulationError::Execution(_) => self.presim_execution_error += 1,
        }
    }
}

/// Snapshot taken right before replaying a base's trailing payment tx, so a
/// later resubmission from the same builder that extends this exact prefix
/// (the common case: an unchanged prefix plus a ratcheted bid, or a few new
/// txs appended before the payment) can resume from here instead of
/// re-executing everything from the parent. Never reused across the payment
/// tx itself: it's expected to change on every resubmission, so it's never
/// part of the cached prefix.
///
/// Reuse requires an exact `gas_limit` match rather than a rebased one:
/// within one slot, `parent_hash`/`timestamp`/`prev_randao` are
/// consensus-fixed, and `base_fee_per_gas` is a deterministic function of
/// `gas_limit` alone (given those), so an exact match guarantees every
/// header field the cached EVM state was executed against is still correct
/// for the new base — no rebasing arithmetic to get subtly wrong.
pub(crate) struct ReplayCheckpoint {
    beneficiary_alloy: Address,
    proposer_fee_recipient: Address,
    parent_hash: B256,
    gas_limit: u64,
    /// Hashes of `base.txs[..base.txs.len() - 1]` (every replayed tx except
    /// the trailing payment), in order — the prefix a later resubmission's
    /// own txs must match, index for index, to extend this checkpoint.
    tx_hashes: Vec<B256>,
    included_tx_hashes: FxHashSet<B256>,
    ctx: PayloadBuildContext,
    streamed: Vec<UpdateChunk>,
}

impl ReplayCheckpoint {
    /// Whether `base` (from `beneficiary_alloy`/`proposer_fee_recipient`,
    /// building on `parent_hash` with `gas_limit`) shares this checkpoint's
    /// full prefix and has at least one more tx (the payment) beyond it.
    fn extends_to(
        &self,
        base: &PreparedBlock,
        beneficiary_alloy: Address,
        proposer_fee_recipient: Address,
        parent_hash: B256,
        gas_limit: u64,
    ) -> bool {
        self.beneficiary_alloy == beneficiary_alloy &&
            self.proposer_fee_recipient == proposer_fee_recipient &&
            self.parent_hash == parent_hash &&
            self.gas_limit == gas_limit &&
            self.tx_hashes.len() < base.txs.len() &&
            self.tx_hashes.iter().zip(base.txs.iter()).all(|(hash, tx)| *hash == tx.hash)
    }
}

/// The block's transaction and receipt tries, kept across a session's emissions so each one
/// re-encodes and re-hashes only the entries since the last. The base's entries are built on
/// their own thread while the first pass extends.
struct OrderedRoots {
    build: Option<std::thread::JoinHandle<Result<OrderedTries, ethrex_trie::TrieError>>>,
    tries: Option<OrderedTries>,
}

struct OrderedTries {
    txs: Trie,
    receipts: Trie,
    /// Entries in both tries.
    len: usize,
    base_blooms: Vec<Bloom>,
}

impl OrderedRoots {
    fn spawn(
        txs: Vec<ethrex_common::types::Transaction>,
        receipts: Vec<ethrex_common::types::Receipt>,
    ) -> Self {
        let build = std::thread::spawn(move || {
            let mut tries = OrderedTries {
                txs: Trie::new_temp(),
                receipts: Trie::new_temp(),
                len: 0,
                base_blooms: receipts
                    .iter()
                    .map(|receipt| bloom_from_logs(&receipt.logs, &NativeCrypto))
                    .collect(),
            };
            let blooms = tries.base_blooms.clone();
            tries.update(0, &txs, &receipts, |ix| blooms[ix])?;
            Ok(tries)
        });
        Self { build: Some(build), tries: None }
    }

    fn get(&mut self) -> Result<&mut OrderedTries, MergeError> {
        if let Some(build) = self.build.take() {
            let built = build
                .join()
                .map_err(|_| MergeError::Internal("ordered roots build panicked".into()))?
                .map_err(|e| MergeError::Internal(e.to_string()))?;
            self.tries = Some(built);
        }
        self.tries.as_mut().ok_or_else(|| MergeError::Internal("ordered roots build failed".into()))
    }
}

impl OrderedTries {
    /// Brings both tries to `txs`/`receipts`, whose first `stable` entries are unchanged since
    /// the last update, and returns the transactions and receipts roots.
    fn update(
        &mut self,
        stable: usize,
        txs: &[ethrex_common::types::Transaction],
        receipts: &[ethrex_common::types::Receipt],
        bloom: impl Fn(usize) -> Bloom,
    ) -> Result<(ethrex_common::H256, ethrex_common::H256), ethrex_trie::TrieError> {
        for (ix, tx) in txs.iter().enumerate().skip(stable.min(self.len)) {
            self.txs.insert(ix.encode_to_vec(), tx.encode_canonical_to_vec())?;
        }
        for (ix, receipt) in receipts.iter().enumerate().skip(stable.min(self.len)) {
            self.receipts.insert(
                ix.encode_to_vec(),
                receipt.encode_inner_with_precomputed_bloom(bloom(ix)),
            )?;
        }
        for ix in txs.len()..self.len {
            self.txs.remove(&ix.encode_to_vec())?;
            self.receipts.remove(&ix.encode_to_vec())?;
        }
        self.len = txs.len();
        Ok((self.txs.hash_no_commit(&NativeCrypto), self.receipts.hash_no_commit(&NativeCrypto)))
    }
}

pub struct MergeSession {
    pub base_block_hash: B256,
    pub base_builder_pubkey: BlsPublicKey,
    /// Base block coinbase == winning builder; the merged block's beneficiary.
    beneficiary: ethrex_common::Address,
    beneficiary_alloy: Address,
    results: reuse::Cache,
    verify_reuse: bool,
    base_value: U256,
    builder_safe: Address,
    /// Live context: base replay + appended orders. Never finalized (emission
    /// finalizes a clone), so the session stays extendable.
    ctx: PayloadBuildContext,
    state: PendingLayer,
    blockchain: Arc<Blockchain>,
    /// Block gas limit minus the reserved distribution gas.
    gas_soft_limit: u64,
    max_blobs: u64,
    blob_count: u64,
    /// All tx hashes in the block so far (base + appended).
    tx_hashes: FxHashSet<B256>,
    /// Versioned hashes of appended blob txs, in append order.
    appended_blobs: Vec<B256>,
    revenues: FxHashMap<Address, OriginRevenue>,
    included_order_ids: Vec<B256>,
    applied_orders: FxHashSet<B256>,
    initial_beneficiary_balance: ethrex_common::U256,
    distribution_gas_limit: u64,
    chain_id: u64,
    best_emitted: U256,
    stats: MergeStats,
    /// Last screening outcome per order, with its priority-fee headroom. Kept
    /// per order rather than per screening: `screen` re-screens the same
    /// order every pass, so counting each screening would multiply-count it.
    order_outcomes: FxHashMap<B256, OrderOutcome>,
    trace: MergeTraceV1,
    /// Wall time the base replay in `activate` took, for the activation log.
    pub replay_us: u64,
    /// Leading base txs that matched the stream's checkpoint, and the
    /// checkpoint's prefix length (0 without a checkpoint).
    pub checkpoint_shared: usize,
    pub checkpoint_len: usize,
    pub base_txs: usize,
    /// Bloom of each live receipt, so emissions hash only the receipts new since.
    receipt_blooms: Vec<Bloom>,
    ordered: OrderedRoots,
    /// Leading entries of the block's txs and receipts unchanged since `ordered` last saw them.
    stable_entries: usize,
    verify_roots: bool,
    /// `i` in the win condition: the base's index in its builder's stream.
    pub base_index: u64,
    /// Value of the base bid, `V(b_i)`.
    pub base_bid_value: U256,
    /// Stamps from base arrival to emission, filled in as the base moves
    /// through the pipeline.
    pub timeline: Timeline,
}

impl MergeSession {
    /// Validates the base block and replays it onto parent state, reusing
    /// `checkpoint` if it extends to this base (see `ReplayCheckpoint`).
    /// Returns the session, a fresh checkpoint for the caller to store back,
    /// and whether this activation was itself a checkpoint hit.
    #[timed]
    pub fn activate(
        slot: &SlotContext,
        base: &PreparedBlock,
        store: &Store,
        blockchain: Arc<Blockchain>,
        relay_config: &RelayConfigV1,
        checkpoint: Option<&ReplayCheckpoint>,
        layer: Option<StateLayer>,
    ) -> Result<(Self, ReplayCheckpoint, bool), MergeError> {
        static SHADOW: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| std::env::var_os("B_SHADOW").is_some());
        if *SHADOW {
            crate::engine::incremental::start_recording();
        }
        let started = Instant::now();
        let replay_start_ns = utcnow_ns();
        let v1 = &base.payload.payload_inner.payload_inner;
        let beneficiary_alloy = v1.fee_recipient;
        let beneficiary = eaddr(beneficiary_alloy);

        // Collateral for the winning builder must exist.
        let builder_safe = relay_config
            .collateral_safe(&beneficiary_alloy)
            .ok_or(MergeError::UnknownCollateral(beneficiary_alloy))?;

        // A base block must have a trailing tx to pay the proposer with. Its
        // `to`/`value` are deliberately not inspected: builders commonly pay
        // through a splitter or disperser contract, so `to` is that contract
        // and `value` is not the bid. The proposer's balance delta across this
        // tx, checked after the replay below, is the real payment proof.
        if base.txs.is_empty() {
            return Err(MergeError::InvalidPayment);
        }

        // Gas headroom for the distribution tx.
        let distribution_gas_limit = relay_config.distribution_gas_limit;
        let base_gas_left = v1.gas_limit.saturating_sub(v1.gas_used);
        if base_gas_left <= distribution_gas_limit {
            return Err(MergeError::InvalidBaseBlock(format!(
                "insufficient gas headroom for distribution: {base_gas_left}"
            )));
        }
        let gas_soft_limit = v1.gas_limit - distribution_gas_limit;

        let chain_config = store.get_chain_config();
        if chain_config.is_amsterdam_activated(v1.timestamp) {
            return Err(MergeError::InvalidBaseBlock(
                "post-Amsterdam blocks are not supported by merging protocol v1".into(),
            ));
        }
        let chain_id = chain_config.chain_id;

        // Blob budget pre-check.
        let max_blobs = chain_config
            .get_fork_blob_schedule(v1.timestamp)
            .map(|schedule| schedule.max as u64)
            .unwrap_or_default();
        let base_blob_count: u64 = base.txs.iter().map(|tx| tx.blob_hashes.len() as u64).sum();
        if base_blob_count > max_blobs {
            return Err(MergeError::InvalidBaseBlock(format!(
                "base block uses {base_blob_count} blobs, max {max_blobs}"
            )));
        }

        let last_ix = base.txs.len() - 1;
        let checkpoint_hit = checkpoint.is_some_and(|ck| {
            ck.extends_to(
                base,
                beneficiary_alloy,
                slot.proposer_fee_recipient,
                v1.parent_hash,
                v1.gas_limit,
            )
        });

        let setup_start = Instant::now();
        let warm_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut ctx, mut tx_hashes, replay_from) = if checkpoint_hit {
            // Reused wholesale: within one slot every header field the
            // checkpoint's state was executed against (parent, timestamp,
            // prev_randao, gas_limit and hence base_fee) is guaranteed
            // unchanged by `extends_to` — see `ReplayCheckpoint`'s doc
            // comment — so nothing here needs re-deriving or re-validating.
            let ck = checkpoint.expect("checked above");
            let mut ctx = ck.ctx.clone();
            ctx.payload.header.extra_data = v1.extra_data.clone().into();
            (ctx, ck.included_tx_hashes.clone(), ck.tx_hashes.len())
        } else {
            // Template block on the parent, pinned to the wire header fields.
            let parent_header = store
                .get_block_header_by_hash(h256(v1.parent_hash))
                .map_err(|e| MergeError::Internal(e.to_string()))?
                .ok_or(MergeError::NotSynced)?;
            let withdrawals: Vec<_> =
                base.payload.payload_inner.withdrawals.iter().map(ewithdrawal).collect();
            let key =
                (v1.parent_hash, v1.timestamp, beneficiary_alloy, v1.prev_randao, v1.gas_limit);
            let cached = slot.templates.lock().ok().and_then(|templates| {
                templates
                    .iter()
                    .find(|(k, w, _)| *k == key && *w == withdrawals)
                    .map(|(_, _, template)| template.clone())
            });
            let template = match cached {
                Some(mut template) => {
                    template.header.extra_data = v1.extra_data.clone().into();
                    template
                }
                None => {
                    let args = BuildPayloadArgs {
                        parent: h256(v1.parent_hash),
                        timestamp: v1.timestamp,
                        fee_recipient: beneficiary,
                        random: h256(v1.prev_randao),
                        withdrawals: Some(withdrawals.clone()),
                        beacon_root: Some(h256(slot.parent_beacon_block_root)),
                        slot_number: None,
                        version: 3,
                        elasticity_multiplier: ELASTICITY_MULTIPLIER,
                        gas_ceil: v1.gas_limit,
                    };
                    let template = create_payload(&args, store, v1.extra_data.clone().into())
                        .map_err(|e| MergeError::Internal(format!("create_payload: {e}")))?;
                    if let Ok(mut templates) = slot.templates.lock() {
                        templates.push((key, withdrawals, template.clone()));
                    }
                    template
                }
            };

            // The derived header must reproduce the wire header exactly; otherwise
            // the base block does not extend our view of the parent.
            if template.header.number != v1.block_number {
                return Err(MergeError::InvalidBaseBlock("block number mismatch".into()));
            }
            if template.header.gas_limit != v1.gas_limit {
                return Err(MergeError::InvalidBaseBlock(format!(
                    "gas limit {} out of bounds (derived {})",
                    v1.gas_limit, template.header.gas_limit
                )));
            }
            let expected_base_fee = calculate_base_fee_per_gas(
                v1.gas_limit,
                parent_header.gas_limit,
                parent_header.gas_used,
                parent_header.base_fee_per_gas.unwrap_or_default(),
                ELASTICITY_MULTIPLIER,
            );
            if expected_base_fee != Some(v1.base_fee_per_gas.to::<u64>()) {
                return Err(MergeError::InvalidBaseBlock("base fee mismatch".into()));
            }
            if template.header.excess_blob_gas.unwrap_or_default() != base.payload.excess_blob_gas {
                return Err(MergeError::InvalidBaseBlock("excess blob gas mismatch".into()));
            }

            let mut ctx = PayloadBuildContext::new(template, store, &blockchain.options.r#type)
                .map_err(|e| MergeError::Internal(format!("payload context: {e}")))?;
            if let Some(recorder) = &slot.recorder {
                let _unused = recorder.parent_header.set(parent_header.clone());
                let _unused = recorder.store.set(store.clone());
                ctx.vm.db.store = recorder.wrap(ctx.vm.db.store.clone());
            }
            let inner = ctx.vm.db.store.clone();
            let fresh = || Arc::new(ethrex_levm::db::CachingDatabase::new(inner.clone(), true));
            ctx.vm.db.store = if v1.parent_hash == slot.parent_hash {
                slot.reads.get_or_init(fresh).clone()
            } else {
                fresh()
            };
            let (warm_store, header, txs, done, results) = (
                ctx.vm.db.store.clone(),
                ctx.payload.header.clone(),
                base.txs.clone(),
                warm_done.clone(),
                slot.results.clone(),
            );
            WARM_POOL.spawn(move || {
                // Cached txs are applied, not run, so only the rest need their state loaded.
                let txs: Vec<_> = txs
                    .iter()
                    .filter(|tx| !results.contains_key(&(beneficiary_alloy, tx.hash)))
                    .map(|tx| (&tx.tx, tx.sender))
                    .collect();
                let _unused = ethrex_vm::backends::levm::LEVM::warm_txs(
                    &txs,
                    &header,
                    warm_store,
                    ethrex_levm::vm::VMType::L1,
                    &NativeCrypto,
                    &|| done.load(std::sync::atomic::Ordering::Relaxed),
                );
            });
            // Wire payloads carry no blob sidecars; blob gas is derived from the
            // tx's versioned hashes (the EVM only needs the hashes).
            ctx.explicit_build = true;

            blockchain
                .apply_system_operations(&mut ctx)
                .map_err(|e| MergeError::Internal(format!("system operations: {e}")))?;
            if *SHADOW {
                let mut db = ctx.vm.db.clone();
                if let Ok(system) = db.get_state_transitions() {
                    crate::engine::incremental::set_system(system);
                }
            }

            (ctx, FxHashSet::default(), 0)
        };
        metrics::stage_latency(
            if checkpoint_hit { "replay_setup_hit" } else { "replay_setup_full" },
            setup_start.elapsed().as_micros() as u64,
        );

        // Replay every base tx from `replay_from` onward, proposer payment
        // included. The proposer's balance delta across the last tx is the
        // only payment proof: it covers a direct transfer and a contract
        // payment alike, and a tx that carries the right fields but reverts
        // shows no delta. A checkpoint snapshot is taken at the same point,
        // right before the payment tx, for a later resubmission to reuse.
        let proposer = eaddr(slot.proposer_fee_recipient);
        let base_fee = ctx.payload.header.base_fee_per_gas;
        let parent_state_root = store
            .get_block_header_by_hash(ctx.payload.header.parent_hash)
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .ok_or(MergeError::NotSynced)?
            .state_root;
        let streamed = match checkpoint {
            Some(ck) if checkpoint_hit => ck.streamed.clone(),
            _ => Vec::new(),
        };
        let (mut feed, state) =
            StateLayer::spawn(store, parent_state_root, streamed, layer, slot.verify_layer);
        let replay_start = Instant::now();
        let mut snapshot_us = 0u64;
        let mut proposer_balance_before_payment = None;
        let mut new_checkpoint = None;
        for (ix, decoded) in base.txs.iter().enumerate().skip(replay_from) {
            if decoded.tx.gas_limit() > ctx.remaining_gas {
                return Err(MergeError::InvalidBaseBlock("base block exceeds gas limit".into()));
            }
            if ix == last_ix || (ix - replay_from) % LAYER_FLUSH_TXS == LAYER_FLUSH_TXS - 1 {
                feed.flush(&mut ctx.vm.db).map_err(|e| MergeError::Internal(e.to_string()))?;
            }
            if ix == last_ix {
                proposer_balance_before_payment = Some(
                    balance_of(&mut ctx.vm, proposer)
                        .map_err(|e| MergeError::Internal(e.to_string()))?,
                );
                let snapshot_start = Instant::now();
                new_checkpoint = Some(ReplayCheckpoint {
                    beneficiary_alloy,
                    proposer_fee_recipient: slot.proposer_fee_recipient,
                    parent_hash: v1.parent_hash,
                    gas_limit: v1.gas_limit,
                    tx_hashes: base.txs[..last_ix].iter().map(|tx| tx.hash).collect(),
                    included_tx_hashes: tx_hashes.clone(),
                    ctx: ctx.clone(),
                    streamed: feed.streamed.clone(),
                });
                snapshot_us = snapshot_start.elapsed().as_micros() as u64;
            }
            let head = HeadTransaction {
                tx: ethrex_common::types::MempoolTransaction::new(
                    decoded.tx.clone(),
                    decoded.sender,
                ),
                tip: decoded.tx.effective_gas_tip(base_fee).unwrap_or_default(),
            };
            reuse::apply_tx(
                "base",
                &slot.results,
                slot.verify_reuse,
                &blockchain,
                &mut ctx,
                head,
                decoded.hash,
                beneficiary_alloy,
                beneficiary,
            )
            .map_err(|err| match err {
                reuse::RunError::Tx(e) => {
                    MergeError::InvalidBaseBlock(format!("base tx failed: {e}"))
                }
                reuse::RunError::Internal(e) => MergeError::Internal(e.to_string()),
            })?;
            tx_hashes.insert(decoded.hash);
        }
        warm_done.store(true, std::sync::atomic::Ordering::Relaxed);
        let shadow_report = crate::engine::incremental::take().map(|effects| {
            let replay_us = replay_start.elapsed().as_micros();
            let exec = crate::engine::incremental::Exec {
                txs: base.txs.iter().map(|tx| (tx.tx.clone(), tx.sender)).collect(),
                header: ctx.payload.header.clone(),
                store: ctx.vm.db.store.clone(),
                coinbase: beneficiary,
                trie_store: store.clone(),
                parent_root: parent_state_root,
                base_hash: base.block_hash,
                pubkey: base.builder_pubkey.0,
                slot: slot.slot,
            };
            let full = replay_from == 0;
            if !full {
                // A base resumed from a checkpoint is not diffed, but what it ran is still known.
                for effect in effects.into_iter().flatten() {
                    crate::engine::incremental::learn((*effect).clone());
                }
                return (None, replay_us);
            }
            (crate::engine::incremental::shadow(beneficiary_alloy, effects, exec), replay_us)
        });
        metrics::stage_latency(
            "replay_txs",
            (replay_start.elapsed().as_micros() as u64).saturating_sub(snapshot_us),
        );
        metrics::stage_latency("replay_snapshot", snapshot_us);
        feed.flush(&mut ctx.vm.db).map_err(|e| MergeError::Internal(e.to_string()))?;
        if let Some((report, replay_us)) = shadow_report {
            match report {
                Some(r) => {
                    let a_root = crate::engine::state_layer::StateLayer::open(
                        store.clone(),
                        parent_state_root,
                    )
                    .and_then(|mut layer| {
                        let all: Vec<_> =
                            feed.streamed.iter().flat_map(|c| c.iter().cloned()).collect();
                        let mut merged: Vec<ethrex_common::types::AccountUpdate> = Vec::new();
                        let mut at: FxHashMap<ethrex_common::Address, usize> = FxHashMap::default();
                        for u in all {
                            match at.get(&u.address) {
                                Some(&ix) => merged[ix].merge(u),
                                None => {
                                    at.insert(u.address, merged.len());
                                    merged.push(u);
                                }
                            }
                        }
                        layer.apply_root(&merged)
                    })
                    .ok();
                    println!(
                        "BSHADOW arrival_ms={} stream_first={} chain={} best_any={} missed={} a_exec={} same_stream={} first={} rebuilt={} txs={} run={} skipped={} delta={} b_us={} a_us={} wrong={} exec_us={} exec_wrong={} exec_failed={} root_us={} root_ok={} run_novel={} run_leads={} content_lead={} causes={}",
                        r.arrival_ms.unwrap_or(i64::MIN),
                        r.stream_first,
                        r.chain,
                        r.best_any.map_or(-1, |b| b as i64),
                        r.missed.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(","),
                        crate::engine::incremental::A_EXECUTED.with(|c| c.get()),
                        r.same_stream.map_or(-1, i32::from),
                        r.first,
                        r.rebuilt,
                        r.txs,
                        r.run,
                        r.skipped,
                        r.delta,
                        r.micros,
                        replay_us,
                        r.wrong,
                        r.exec_micros,
                        r.exec_wrong,
                        r.exec_failed,
                        r.root_micros,
                        if r.root.is_some() && r.root == a_root {
                            "true".to_string()
                        } else {
                            format!(
                                "false(b_some={},a_some={})",
                                r.root.is_some(),
                                a_root.is_some()
                            )
                        },
                        r.run_novel,
                        r.run_leads.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(","),
                        r.content_lead.map_or(-1, |l| l as i64),
                        r.causes.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
                    );
                }
                None => println!("BSHADOW none"),
            }
        }
        let accounts: usize = feed.streamed.iter().map(|chunk| chunk.len()).sum();
        let slots: usize = feed
            .streamed
            .iter()
            .flat_map(|chunk| chunk.iter())
            .map(|u| u.added_storage.len())
            .sum();
        metrics::account_updates("base", accounts, slots);
        drop(feed);
        let new_checkpoint = new_checkpoint
            .expect("base.txs is non-empty (checked above), so last_ix is always visited");
        debug!(
            base_block_hash = %base.block_hash,
            txs = base.txs.len(),
            replayed = base.txs.len() - replay_from,
            checkpoint_hit,
            gas_used = ctx.gas_used(),
            "replayed base block"
        );
        if let Some(recorder) = &slot.recorder {
            recorder.base_gas(
                base.block_hash,
                base.txs
                    .iter()
                    .map(|tx| tx.hash)
                    .zip(ctx.receipts.iter().map(|r| r.cumulative_gas_used)),
            );
        }
        if ctx.gas_used() != v1.gas_used {
            return Err(MergeError::InvalidBaseBlock(format!(
                "base block gas mismatch: declared {}, executed {}",
                v1.gas_used,
                ctx.gas_used()
            )));
        }
        let proposer_balance_after_payment =
            balance_of(&mut ctx.vm, proposer).map_err(|e| MergeError::Internal(e.to_string()))?;
        let paid = au256(
            proposer_balance_after_payment
                .saturating_sub(proposer_balance_before_payment.expect("set on last iteration")),
        );
        if paid < base.block_value {
            return Err(MergeError::InvalidPayment);
        }

        // Merged revenue is measured as the beneficiary balance delta from
        // this point on (after the base replay, matching the simulator).
        let initial_beneficiary_balance = balance_of(&mut ctx.vm, beneficiary)
            .map_err(|e| MergeError::Internal(e.to_string()))?;

        let ordered =
            OrderedRoots::spawn(ctx.payload.body.transactions.clone(), ctx.receipts.clone());
        let stable_entries = ctx.receipts.len();
        let session = Self {
            base_block_hash: base.block_hash,
            base_builder_pubkey: base.builder_pubkey,
            beneficiary,
            beneficiary_alloy,
            results: slot.results.clone(),
            verify_reuse: slot.verify_reuse,
            base_value: base.block_value,
            builder_safe,
            ctx,
            state,
            blockchain,
            gas_soft_limit,
            max_blobs,
            blob_count: base_blob_count,
            tx_hashes,
            appended_blobs: Vec::new(),
            revenues: FxHashMap::default(),
            included_order_ids: Vec::new(),
            applied_orders: FxHashSet::default(),
            initial_beneficiary_balance,
            distribution_gas_limit,
            chain_id,
            best_emitted: U256::ZERO,
            stats: MergeStats::default(),
            order_outcomes: FxHashMap::default(),
            trace: MergeTraceV1 { base_block_recv_ns: base.recv_ns, ..Default::default() },
            replay_us: started.elapsed().as_micros() as u64,
            checkpoint_shared: checkpoint
                .map(|ck| {
                    ck.tx_hashes
                        .iter()
                        .zip(base.txs.iter())
                        .take_while(|(h, tx)| **h == tx.hash)
                        .count()
                })
                .unwrap_or_default(),
            checkpoint_len: checkpoint.map(|ck| ck.tx_hashes.len()).unwrap_or_default(),
            base_txs: base.txs.len(),
            receipt_blooms: Vec::new(),
            ordered,
            stable_entries,
            verify_roots: slot.verify_layer,
            base_index: base.submission_index,
            base_bid_value: base.block_value,
            timeline: Timeline {
                recv_ns: base.recv_ns,
                ingest_done_ns: base.ingest_done_ns,
                replay_start_ns,
                replay_end_ns: utcnow_ns(),
                ..Default::default()
            },
        };
        metrics::stage_latency(
            if checkpoint_hit { "replay_checkpoint_hit" } else { "replay_full" },
            session.replay_us,
        );
        metrics::base_age("replay", utcnow_ns().saturating_sub(base.recv_ns) / 1_000_000);
        Ok((session, new_checkpoint, checkpoint_hit))
    }

    /// Picks the pool orders worth presimulating. Callers hold the pool's read
    /// lock only for this, so ingest never waits behind a presim.
    #[timed]
    pub fn screen(
        &mut self,
        orders: &[Arc<PreparedOrder>],
        excluded: &FxHashSet<B256>,
    ) -> Vec<Arc<PreparedOrder>> {
        self.trace.sim_start_ns = utcnow_ns();
        let screen_start = Instant::now();
        let base_fee = self.ctx.payload.header.base_fee_per_gas;
        self.stats.orders_excluded_skipped =
            orders.iter().filter(|order| excluded.contains(&order.order_hash)).count() as u64;
        for order in orders.iter().filter(|order| excluded.contains(&order.order_hash)) {
            self.record_outcome(order, "excluded", Some(UnmergedReason::Replaced), base_fee);
        }

        let mut candidates = Vec::new();
        for order in orders {
            if excluded.contains(&order.order_hash) ||
                self.applied_orders.contains(&order.order_id) ||
                order.source_block_hash == self.base_block_hash
            {
                continue;
            }
            match simulate::gate_order(
                order,
                &self.tx_hashes,
                self.available_gas(),
                self.available_blobs(),
            ) {
                Ok(()) => candidates.push(order.clone()),
                Err(err) => self.record_outcome(
                    order,
                    sim_error_label(&err),
                    Some(err.unmerged_reason()),
                    base_fee,
                ),
            }
        }
        metrics::stage_latency("extend_screen", screen_start.elapsed().as_micros() as u64);
        metrics::extend_orders("candidates", candidates.len());
        candidates
    }

    /// Presimulates `candidates` in parallel, then greedily applies them
    /// best-payment-first to the live context. Returns whether the block
    /// changed. Port of `append_greedily_until_gas_limit`.
    #[timed]
    pub fn try_extend(&mut self, candidates: Vec<Arc<PreparedOrder>>) -> bool {
        if candidates.is_empty() {
            return false;
        }
        self.ctx.vm.db.keep_tx_backup = true;
        let header = self.ctx.payload.header.clone();
        let base_fee = header.base_fee_per_gas;

        self.stats.candidates_screened += candidates.len() as u64;
        let available_gas = self.available_gas();
        let available_blobs = self.available_blobs();
        let vm = &self.ctx.vm;
        let beneficiary = self.beneficiary;
        let (results, verify, builder) = (&self.results, self.verify_reuse, self.beneficiary_alloy);
        let presim_start = Instant::now();
        let base = Arc::new(simulate::PresimBase::new(&vm.db));
        metrics::stage_latency("extend_presim_base", presim_start.elapsed().as_micros() as u64);
        let results: Vec<Result<SimulatedOrder, SimulationError>> = PRESIM_POOL.install(|| {
            use rayon::prelude::*;
            candidates
                .par_iter()
                .enumerate()
                .map(|(ix, order)| {
                    let mut vm = base.evm(vm);
                    simulate::simulate_order(
                        &mut vm,
                        &header,
                        order,
                        ix,
                        available_gas,
                        available_blobs,
                        beneficiary,
                        (results, verify, builder),
                    )
                })
                .collect()
        });
        let mut simulated: Vec<SimulatedOrder> = Vec::with_capacity(results.len());
        for (result, order) in results.into_iter().zip(&candidates) {
            match result {
                Ok(order) => simulated.push(order),
                Err(err) => {
                    self.stats.count_sim_error(&err);
                    self.record_outcome(
                        order,
                        sim_error_label(&err),
                        Some(err.unmerged_reason()),
                        base_fee,
                    );
                    debug!(order = %order.order_id, %err, "order presim discarded");
                }
            }
        }

        // Highest payment first.
        simulated.sort_unstable_by_key(|s| std::cmp::Reverse(s.builder_payment));
        metrics::stage_latency("extend_presim", presim_start.elapsed().as_micros() as u64);
        metrics::extend_orders("presim_ok", simulated.len());

        let apply_start = Instant::now();
        let mut applied = 0;
        let mut changed = false;
        for candidate in simulated {
            let order = &candidates[candidate.order_ix];
            match self.try_apply(order, &candidate.include_tx) {
                Ok(true) => {
                    changed = true;
                    applied += 1;
                    self.record_outcome(order, "applied", None, base_fee);
                }
                Ok(false) => {
                    self.record_outcome(
                        order,
                        "apply_rejected",
                        Some(UnmergedReason::Invalid),
                        base_fee,
                    );
                }
                Err(err) => {
                    self.record_outcome(
                        order,
                        "apply_error",
                        Some(UnmergedReason::Invalid),
                        base_fee,
                    );
                    debug!(order = %order.order_id, %err, "order apply skipped");
                }
            }
        }

        metrics::stage_latency("extend_apply", apply_start.elapsed().as_micros() as u64);
        metrics::extend_orders("applied", applied);

        self.trace.sim_end_ns = utcnow_ns();
        metrics::stage_latency(
            "extend",
            self.trace.sim_end_ns.saturating_sub(self.trace.sim_start_ns) / 1000,
        );
        changed
    }

    fn record_outcome(
        &mut self,
        order: &PreparedOrder,
        label: &'static str,
        reason: Option<UnmergedReason>,
        base_fee: Option<u64>,
    ) {
        let headroom = order_headroom(order, base_fee);
        match self.order_outcomes.entry(order.order_id) {
            Entry::Occupied(mut entry) => {
                let outcome = entry.get_mut();
                outcome.label = label;
                outcome.reason = reason;
                outcome.headroom = headroom;
            }
            Entry::Vacant(entry) => {
                entry.insert(OrderOutcome {
                    label,
                    reason,
                    headroom,
                    txs: order.txs.iter().map(|tx| tx.hash).collect(),
                });
            }
        }
    }

    /// Applies `order` with the presim's tx selection and keeps it only if it
    /// still pays the beneficiary. Rolls the context back on any violation.
    #[timed]
    fn try_apply(
        &mut self,
        order: &PreparedOrder,
        include_tx: &[bool],
    ) -> Result<bool, MergeError> {
        if simulate::gate_order(
            order,
            &self.tx_hashes,
            self.available_gas(),
            self.available_blobs(),
        )
        .is_err()
        {
            return Ok(false);
        }

        let txs_start = Instant::now();
        let initial_balance = simulate::balance_of(&mut self.ctx.vm, self.beneficiary)
            .map_err(|e| MergeError::Internal(e.to_string()))?;
        let mut tx_backups = Vec::with_capacity(order.txs.len());
        let scalar_snapshot = (
            self.ctx.remaining_gas,
            self.ctx.cumulative_gas_spent,
            self.ctx.block_value,
            self.ctx.payload_size,
            self.ctx.payload.header.blob_gas_used,
            self.ctx.payload.body.transactions.len(),
            self.ctx.receipts.len(),
        );
        let blob_snapshot = (self.blob_count, self.appended_blobs.len());

        let base_fee = self.ctx.payload.header.base_fee_per_gas;
        let mut applied_hashes = Vec::new();
        let mut rollback = false;
        for (i, decoded) in order.txs.iter().enumerate() {
            if !include_tx[i] {
                continue;
            }
            if decoded.gas_limit > self.available_gas() ||
                decoded.blob_hashes.len() as u64 > self.available_blobs()
            {
                if order.can_drop(i) {
                    continue;
                }
                rollback = true;
                break;
            }
            let head = HeadTransaction {
                tx: ethrex_common::types::MempoolTransaction::new(
                    decoded.tx.clone(),
                    decoded.sender,
                ),
                tip: decoded.tx.effective_gas_tip(base_fee).unwrap_or_default(),
            };
            match reuse::apply_tx(
                "order",
                &self.results,
                self.verify_reuse,
                &self.blockchain,
                &mut self.ctx,
                head,
                decoded.hash,
                self.beneficiary_alloy,
                self.beneficiary,
            ) {
                Ok(()) => {
                    tx_backups.extend(self.ctx.vm.db.tx_backup.take());
                    let succeeded = self.ctx.receipts.last().map(|r| r.succeeded).unwrap_or(false);
                    if !succeeded && !order.can_revert(i) {
                        rollback = true;
                        break;
                    }
                    applied_hashes.push(decoded.hash);
                    self.blob_count += decoded.blob_hashes.len() as u64;
                    self.appended_blobs.extend(decoded.blob_hashes.iter().copied());
                }
                Err(reuse::RunError::Internal(e)) => {
                    return Err(MergeError::Internal(e.to_string()))
                }
                Err(_) if order.can_drop(i) => {}
                Err(_) => {
                    rollback = true;
                    break;
                }
            }
            if self.ctx.gas_used() > self.gas_soft_limit {
                rollback = true;
                break;
            }
        }

        let builder_payment = au256(
            simulate::balance_of(&mut self.ctx.vm, self.beneficiary)
                .map_err(|e| MergeError::Internal(e.to_string()))?
                .saturating_sub(initial_balance),
        );
        metrics::stage_latency("apply_txs", txs_start.elapsed().as_micros() as u64);

        if rollback || applied_hashes.is_empty() || builder_payment.is_zero() {
            let rollback_start = Instant::now();
            if rollback {
                self.stats.apply_rollbacks += 1;
            }
            for backup in tx_backups.into_iter().rev() {
                self.ctx.vm.db.tx_backup = Some(backup);
                self.ctx.vm.undo_last_tx().map_err(|e| MergeError::Internal(e.to_string()))?;
            }
            self.ctx.vm.db.tx_backup = None;
            let (remaining, cumulative, value, size, blob_gas, tx_len, receipts_len) =
                scalar_snapshot;
            self.ctx.remaining_gas = remaining;
            self.ctx.cumulative_gas_spent = cumulative;
            self.ctx.block_value = value;
            self.ctx.payload_size = size;
            self.ctx.payload.header.blob_gas_used = blob_gas;
            self.ctx.payload.body.transactions.truncate(tx_len);
            self.ctx.receipts.truncate(receipts_len);
            self.stable_entries = self.stable_entries.min(receipts_len);
            self.blob_count = blob_snapshot.0;
            self.appended_blobs.truncate(blob_snapshot.1);
            metrics::stage_latency("apply_rollback", rollback_start.elapsed().as_micros() as u64);
            return Ok(false);
        }

        // Commit bookkeeping.
        self.stats.orders_applied += 1;
        self.tx_hashes.extend(applied_hashes.iter().copied());
        self.applied_orders.insert(order.order_id);
        self.included_order_ids.push(order.order_id);
        let entry = self.revenues.entry(order.origin).or_insert_with(|| OriginRevenue {
            revenue: U256::ZERO,
            txs: Vec::new(),
            pubkey: order.builder_pubkey,
        });
        entry.revenue += builder_payment;
        entry.txs.extend(applied_hashes);
        let updates = self
            .ctx
            .vm
            .db
            .get_state_transitions_tx()
            .map_err(|e| MergeError::Internal(e.to_string()))?;
        self.state.send_orders(Arc::new(updates));
        Ok(true)
    }

    /// Builds the distribution tx on a clone of the live context, finalizes it
    /// and assembles the `MergedBlockV1`. Distinguishes "nothing to emit" from
    /// "improvement blocked by the spacing gate" so the worker can retry the
    /// latter.
    #[timed]
    pub fn emit(
        &mut self,
        slot_number: u64,
        proposer_fee_recipient: Address,
        relay_config: &RelayConfigV1,
        engine_config: &EngineConfig,
    ) -> Result<EmitOutcome, MergeError> {
        let emit_start_ns = utcnow_ns();
        let total_revenue: U256 = self.revenues.values().map(|v| v.revenue).sum();
        if total_revenue.is_zero() {
            self.stats.emit_no_revenue += 1;
            return Ok(EmitOutcome::NotImproved);
        }

        // Sanity: revenue accounting must match the beneficiary balance delta.
        let current_balance = balance_of(&mut self.ctx.vm, self.beneficiary)
            .map_err(|e| MergeError::Internal(e.to_string()))?;
        let delta = au256(current_balance.saturating_sub(self.initial_beneficiary_balance));
        if delta != total_revenue {
            return Err(MergeError::BalanceDeltaMismatch { revenues: total_revenue, delta });
        }

        let base_fee = self.ctx.payload.header.base_fee_per_gas.unwrap_or_default();
        let estimated_payment_cost =
            U256::from(base_fee).saturating_mul(U256::from(self.distribution_gas_limit));
        if total_revenue <= estimated_payment_cost {
            self.stats.emit_no_revenue += 1;
            return Ok(EmitOutcome::NotImproved);
        }

        let distribution = DistributionConfig::from_relay_config(relay_config);
        let updated_revenues = payment::prepare_revenues(
            &distribution,
            &self.revenues,
            estimated_payment_cost,
            proposer_fee_recipient,
            relay_config.relay_fee_recipient,
            self.beneficiary_alloy,
        );
        let proposer_added_value =
            updated_revenues.get(&proposer_fee_recipient).cloned().unwrap_or_default();
        let proposer_value = self.base_value + proposer_added_value;

        // Winning builder must get something (indirectly checked by
        // prepare_revenues, kept as an explicit guard like the simulator).
        let winning_builder_revenue = total_revenue
            .saturating_sub(updated_revenues.values().sum())
            .saturating_sub(estimated_payment_cost);
        if winning_builder_revenue.is_zero() {
            self.stats.emit_no_revenue += 1;
            return Ok(EmitOutcome::NotImproved);
        }

        if proposer_value <= self.best_emitted + engine_config.min_value_increase_wei {
            self.stats.emit_not_improved += 1;
            return Ok(EmitOutcome::NotImproved);
        }

        // Finalization clears the vm caches, so it runs on a clone; the live
        // session stays extendable.
        metrics::stage_latency("emit_prepare", utcnow_ns().saturating_sub(emit_start_ns) / 1000);
        // Live receipts below the last emission's count are never rolled back,
        // so their blooms stay valid across emissions.
        if self.receipt_blooms.is_empty() {
            self.receipt_blooms = std::mem::take(&mut self.ordered.get()?.base_blooms);
        }
        self.receipt_blooms.truncate(self.ctx.receipts.len());
        for receipt in &self.ctx.receipts[self.receipt_blooms.len()..] {
            self.receipt_blooms.push(bloom_from_logs(&receipt.logs, &NativeCrypto));
        }
        let clone_start = Instant::now();
        let mut ctx = self.ctx.clone();
        metrics::stage_latency("emit_clone", clone_start.elapsed().as_micros() as u64);
        let payment_start = Instant::now();

        let safe = eaddr(self.builder_safe);
        let safe_balance =
            balance_of(&mut ctx.vm, safe).map_err(|e| MergeError::Internal(e.to_string()))?;
        // Safe nonce is stored at slot 5. Prefer the block's cached state (a
        // base/merged tx may have touched the Safe), fall back to the store.
        let nonce_slot = ethrex_common::H256::from_low_u64_be(5);
        let cached_nonce = ctx
            .vm
            .db
            .get_account(safe)
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .storage
            .get(&nonce_slot)
            .copied()
            .or_else(|| {
                ctx.vm.db.initial_accounts_state.get(&safe)?.storage.get(&nonce_slot).copied()
            });
        let safe_nonce = match cached_nonce {
            Some(value) => value,
            None => ctx
                .vm
                .db
                .store
                .get_storage_value(safe, nonce_slot)
                .map_err(|e| MergeError::Internal(e.to_string()))?,
        }
        .as_u64();
        let signer_address = eaddr(engine_config.relay_signer.address());
        let signer_nonce = ctx
            .vm
            .db
            .get_account(signer_address)
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .info
            .nonce;

        let inputs = PaymentInputs {
            safe: self.builder_safe,
            safe_balance: au256(safe_balance),
            safe_nonce,
            signer_nonce,
            chain_id: self.chain_id,
            gas_limit: self.distribution_gas_limit,
            base_fee_per_gas: base_fee as u128,
            multisend_contract: relay_config.multisend_contract,
        };
        let encoded =
            payment::build_payment_tx(&engine_config.relay_signer, &inputs, &updated_revenues)?;
        let payment_tx = ethrex_common::types::Transaction::decode_canonical(&encoded)
            .map_err(|e| MergeError::Internal(format!("payment tx decode: {e}")))?;
        let payment_sender = payment_tx
            .sender(&NativeCrypto)
            .map_err(|e| MergeError::Internal(format!("payment tx sender: {e}")))?;

        let head = HeadTransaction {
            tx: ethrex_common::types::MempoolTransaction::new(payment_tx, payment_sender),
            tip: ethrex_common::U256::zero(),
        };
        self.blockchain
            .apply_tx_to_payload(head, &mut ctx)
            .map_err(|e| MergeError::Internal(format!("payment tx failed: {e}")))?;
        if !ctx.receipts.last().map(|r| r.succeeded).unwrap_or(false) {
            return Err(MergeError::RevenueAllocationReverted);
        }

        metrics::stage_latency("emit_payment", payment_start.elapsed().as_micros() as u64);
        let finalize_start = Instant::now();

        self.blockchain
            .extract_requests(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("extract requests: {e}")))?;
        self.blockchain
            .apply_withdrawals(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("apply withdrawals: {e}")))?;
        metrics::stage_latency("emit_requests", finalize_start.elapsed().as_micros() as u64);
        let state_root_start = Instant::now();
        let block_access_list = ctx.vm.take_bal();
        let account_updates =
            ctx.vm.get_state_transitions().map_err(|e| MergeError::Internal(e.to_string()))?;
        let state_root = self
            .state
            .root(account_updates.clone())
            .map_err(|e| MergeError::Internal(format!("state root: {e}")))?;
        let receipts_start = Instant::now();
        let blooms: Vec<Bloom> = ctx
            .receipts
            .iter()
            .enumerate()
            .map(|(ix, receipt)| {
                self.receipt_blooms
                    .get(ix)
                    .copied()
                    .unwrap_or_else(|| bloom_from_logs(&receipt.logs, &NativeCrypto))
            })
            .collect();
        let logs_bloom = blooms.iter().fold(Bloom::zero(), |acc, bloom| acc | *bloom);
        let stable = self.stable_entries;
        let (transactions_root, receipts_root) = self
            .ordered
            .get()?
            .update(stable, &ctx.payload.body.transactions, &ctx.receipts, |ix| blooms[ix])
            .map_err(|e| MergeError::Internal(format!("ordered roots: {e}")))?;
        if self.verify_roots {
            let full_txs = ethrex_common::types::compute_transactions_root(
                &ctx.payload.body.transactions,
                &NativeCrypto,
            );
            let full_receipts = Trie::compute_hash_from_unsorted_iter(
                ctx.receipts.iter().enumerate().map(|(ix, receipt)| {
                    (ix.encode_to_vec(), receipt.encode_inner_with_precomputed_bloom(blooms[ix]))
                }),
                &NativeCrypto,
            );
            metrics::layer_build(
                if (full_txs, full_receipts) == (transactions_root, receipts_root) {
                    "ordered_verified"
                } else {
                    "ordered_mismatch"
                },
            );
        }
        self.stable_entries = self.ctx.receipts.len();
        metrics::stage_latency("emit_receipts_root", receipts_start.elapsed().as_micros() as u64);
        self.blockchain
            .finalize_payload_with_state_root(
                &mut ctx,
                state_root,
                (transactions_root, receipts_root, logs_bloom),
                account_updates,
                block_access_list,
            )
            .map_err(|e| MergeError::Internal(format!("finalize payload: {e}")))?;
        metrics::stage_latency("emit_state_root", state_root_start.elapsed().as_micros() as u64);

        metrics::stage_latency("emit_finalize", finalize_start.elapsed().as_micros() as u64);
        {
            let slots: usize = ctx.account_updates.iter().map(|u| u.added_storage.len()).sum();
            metrics::account_updates("delta", ctx.account_updates.len(), slots);
        }
        let encode_start = Instant::now();
        self.trace.finalize_ns = utcnow_ns();
        metrics::emit_value(total_revenue, proposer_added_value);
        metrics::base_age(
            "emit",
            self.trace.finalize_ns.saturating_sub(self.trace.base_block_recv_ns) / 1_000_000,
        );
        metrics::stage_latency("emit", self.trace.finalize_ns.saturating_sub(emit_start_ns) / 1000);

        let execution_payload = block_to_payload_v3(&ctx.payload);
        let execution_requests = requests_to_v4(ctx.requests.as_deref().unwrap_or_default())
            .map_err(|e| MergeError::Internal(format!("execution requests: {e}")))?;

        metrics::stage_latency("emit_encode", encode_start.elapsed().as_micros() as u64);
        let builder_inclusions = self
            .revenues
            .iter()
            .map(|(origin, revenue)| BuilderInclusion {
                builder_pubkey: revenue.pubkey,
                origin_coinbase: *origin,
                contribution: revenue.revenue,
                revenue: updated_revenues.get(origin).cloned().unwrap_or_default(),
                txs: revenue.txs.clone(),
            })
            .collect();
        let relay_revenue =
            updated_revenues.get(&relay_config.relay_fee_recipient).cloned().unwrap_or_default();

        self.best_emitted = proposer_value;
        self.stats.emissions += 1;

        debug!(
            base_block_hash = %self.base_block_hash,
            %proposer_value,
            %total_revenue,
            appended_txs = self.included_order_ids.len(),
            "merged block finalized"
        );

        Ok(EmitOutcome::Emitted(Box::new(MergedBlockV1 {
            slot: slot_number,
            response_id: 0, // stamped by the server tile
            base_block_hash: self.base_block_hash,
            base_builder_pubkey: self.base_builder_pubkey,
            execution_payload,
            execution_requests,
            appended_blobs: self.appended_blobs.clone(),
            proposer_value,
            base_builder_revenue: winning_builder_revenue,
            relay_revenue,
            builder_inclusions,
            included_order_ids: self.included_order_ids.clone(),
            trace: self.trace,
            unmerged_txs: self
                .order_outcomes
                .values()
                .filter_map(|outcome| outcome.reason.map(|reason| (outcome, reason)))
                .flat_map(|(outcome, reason)| {
                    outcome.txs.iter().map(move |&tx_hash| UnmergedTx { tx_hash, reason })
                })
                .collect(),
        })))
    }

    fn available_gas(&self) -> u64 {
        self.gas_soft_limit.saturating_sub(self.ctx.gas_used())
    }

    fn available_blobs(&self) -> u64 {
        self.max_blobs.saturating_sub(self.blob_count)
    }

    pub fn take_layer(&mut self) -> Option<StateLayer> {
        self.state.take()
    }

    #[cfg(test)]
    pub fn stats(&self) -> &MergeStats {
        &self.stats
    }

    /// One structured summary line, emitted when the session is finally
    /// discarded (slot end, connection reset, parked-eviction).
    pub fn log_stats(&self, reason: &str) {
        let mut applied = U256::ZERO;
        let mut lost_revert = U256::ZERO;
        let mut lost_total = U256::ZERO;
        for outcome in self.order_outcomes.values() {
            let headroom = outcome.headroom;
            metrics::order_outcome(outcome.label, headroom);
            match outcome.label {
                "applied" => applied = applied.saturating_add(headroom),
                "revert_not_allowed" => {
                    lost_revert = lost_revert.saturating_add(headroom);
                    lost_total = lost_total.saturating_add(headroom);
                }
                _ => lost_total = lost_total.saturating_add(headroom),
            }
        }
        let t = &self.timeline;
        let ms = |from: u64, to: u64| {
            if from == 0 || to == 0 { 0 } else { to.saturating_sub(from) / 1_000_000 }
        };
        info!(
            reason,
            base_block_hash = %self.base_block_hash,
            builder = %self.beneficiary_alloy,
            base_index = self.base_index,
            wait_ms = ms(t.recv_ns, t.replay_start_ns),
            cycle_ms = ms(t.replay_start_ns, t.first_emit_ns),
            first_emit_age_ms = ms(t.recv_ns, t.first_emit_ns),
            best_emitted = %self.best_emitted,
            orders_included = self.included_order_ids.len(),
            candidates_screened = self.stats.candidates_screened,
            applied = self.stats.orders_applied,
            rollbacks = self.stats.apply_rollbacks,
            zero_payment = self.stats.presim_zero_payment,
            out_of_gas = self.stats.presim_out_of_gas,
            out_of_blobs = self.stats.presim_out_of_blobs,
            duplicate = self.stats.presim_duplicate,
            revert_not_allowed = self.stats.presim_revert_not_allowed,
            drop_not_allowed = self.stats.presim_drop_not_allowed,
            execution_error = self.stats.presim_execution_error,
            emissions = self.stats.emissions,
            emit_not_improved = self.stats.emit_not_improved,
            emit_no_revenue = self.stats.emit_no_revenue,
            orders_seen = self.order_outcomes.len(),
            value_applied_gwei = metrics::gwei(applied),
            value_lost_revert_gwei = metrics::gwei(lost_revert),
            value_lost_total_gwei = metrics::gwei(lost_total),
            "merge session stats"
        );
    }
}
