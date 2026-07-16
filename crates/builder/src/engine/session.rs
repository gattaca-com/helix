//! Per-base-block merge session: validates and replays the activated base
//! block onto parent state, greedily appends profitable orders, and emits
//! improved `MergedBlockV1`s with the Safe multiSend distribution appended.
//! Port of the simulator's `merge_block` / `BlockBuilder` /
//! `append_greedily_until_gas_limit` (`crates/simulator/src/block_merging/mod.rs`)
//! onto ethrex's `PayloadBuildContext`.

use std::{sync::Arc, time::Instant};

use alloy_eips::eip7825::MAX_TX_GAS_LIMIT_OSAKA;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::beacon::BlsPublicKey;
use ethrex_blockchain::{
    Blockchain,
    payload::{BuildPayloadArgs, HeadTransaction, PayloadBuildContext, create_payload},
};
use ethrex_common::types::{ELASTICITY_MULTIPLIER, TxKind, calculate_base_fee_per_gas};
use ethrex_crypto::native::NativeCrypto;
use ethrex_storage::Store;
use helix_tcp_types::merging::{
    builder_to_relay::{BuilderInclusion, MergeTraceV1, MergedBlockV1},
    control::RelayConfigV1,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, info};

use crate::{
    engine::{
        convert::{au256, block_to_payload_v3, eaddr, ewithdrawal, h256, requests_to_v4},
        error::{MergeError, SimulationError},
        payment::{self, DistributionConfig, PaymentInputs},
        simulate::{self, balance_of},
        types::{
            EngineConfig, OriginRevenue, PreparedBlock, PreparedOrder, SimulatedOrder, SlotState,
        },
    },
    utils::utcnow_ns,
};

/// Result of an emission attempt.
pub enum EmitOutcome {
    Emitted(Box<MergedBlockV1>),
    /// No merged revenue, no improvement over the last emission, or the
    /// improvement doesn't cover the distribution cost; nothing to retry.
    NotImproved,
    /// An improvement exists but the emission-spacing gate blocked it; worth
    /// retrying once the window passes even with no further inbound events.
    Throttled,
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
    pub emit_throttled: u64,
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

pub struct MergeSession {
    pub base_block_hash: B256,
    pub base_builder_pubkey: BlsPublicKey,
    /// Base block coinbase == winning builder; the merged block's beneficiary.
    beneficiary: ethrex_common::Address,
    beneficiary_alloy: Address,
    base_value: U256,
    builder_safe: Address,
    /// Live context: base replay + appended orders. Never finalized (emission
    /// finalizes a clone), so the session stays extendable.
    ctx: PayloadBuildContext,
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
    /// EIP-7825 per-transaction gas cap at the block's timestamp.
    max_tx_gas_limit: u64,
    chain_id: u64,
    best_emitted: U256,
    last_emit: Option<Instant>,
    /// A throttled improvement is waiting; the worker retries after the
    /// spacing window even with no further inbound events.
    pub pending_emission: bool,
    stats: MergeStats,
    trace: MergeTraceV1,
}

impl MergeSession {
    /// Validates the base block and replays it onto parent state.
    pub fn activate(
        slot: &SlotState,
        base: &PreparedBlock,
        store: &Store,
        blockchain: Arc<Blockchain>,
        relay_config: &RelayConfigV1,
    ) -> Result<Self, MergeError> {
        let v1 = &base.payload.payload_inner.payload_inner;
        let beneficiary_alloy = v1.fee_recipient;
        let beneficiary = eaddr(beneficiary_alloy);

        // Collateral for the winning builder must exist.
        let builder_safe = relay_config
            .collateral_safe(&beneficiary_alloy)
            .ok_or(MergeError::UnknownCollateral(beneficiary_alloy))?;

        // The trailing tx must be the proposer payment of exactly block_value.
        // It is kept in place; the distribution tx is appended separately.
        let last_tx = base.txs.last().ok_or(MergeError::InvalidPayment)?;
        let pays_proposer =
            matches!(last_tx.tx.to(), TxKind::Call(to) if to == eaddr(slot.proposer_fee_recipient));
        if !pays_proposer || au256(last_tx.tx.value()) != base.block_value {
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
        // EIP-7825: the per-transaction gas cap applies from Osaka onwards
        // (Amsterdam, with its different cap, is rejected above).
        let max_tx_gas_limit = if chain_config.is_osaka_activated(v1.timestamp) {
            MAX_TX_GAS_LIMIT_OSAKA
        } else {
            u64::MAX
        };

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

        // Template block on the parent, pinned to the wire header fields.
        let parent_header = store
            .get_block_header_by_hash(h256(v1.parent_hash))
            .map_err(|e| MergeError::Internal(e.to_string()))?
            .ok_or(MergeError::NotSynced)?;
        let args = BuildPayloadArgs {
            parent: h256(v1.parent_hash),
            timestamp: v1.timestamp,
            fee_recipient: beneficiary,
            random: h256(v1.prev_randao),
            withdrawals: Some(
                base.payload.payload_inner.withdrawals.iter().map(ewithdrawal).collect(),
            ),
            beacon_root: Some(h256(slot.parent_beacon_block_root)),
            slot_number: None,
            version: 3,
            elasticity_multiplier: ELASTICITY_MULTIPLIER,
            gas_ceil: v1.gas_limit,
        };
        let template = create_payload(&args, store, v1.extra_data.clone().into())
            .map_err(|e| MergeError::Internal(format!("create_payload: {e}")))?;

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
        // Wire payloads carry no blob sidecars; blob gas is derived from the
        // tx's versioned hashes (the EVM only needs the hashes).
        ctx.explicit_build = true;

        blockchain
            .apply_system_operations(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("system operations: {e}")))?;

        // Replay every base tx, proposer payment included.
        let mut tx_hashes = FxHashSet::default();
        let base_fee = ctx.payload.header.base_fee_per_gas;
        for decoded in base.txs.iter() {
            if decoded.tx.gas_limit() > ctx.remaining_gas {
                return Err(MergeError::InvalidBaseBlock("base block exceeds gas limit".into()));
            }
            let head = HeadTransaction {
                tx: ethrex_common::types::MempoolTransaction::new(
                    decoded.tx.clone(),
                    decoded.sender,
                ),
                tip: decoded.tx.effective_gas_tip(base_fee).unwrap_or_default(),
            };
            blockchain
                .apply_tx_to_payload(head, &mut ctx)
                .map_err(|e| MergeError::InvalidBaseBlock(format!("base tx failed: {e}")))?;
            tx_hashes.insert(decoded.hash);
        }
        debug!(
            base_block_hash = %base.block_hash,
            txs = base.txs.len(),
            gas_used = ctx.gas_used(),
            "replayed base block"
        );
        if ctx.gas_used() != v1.gas_used {
            return Err(MergeError::InvalidBaseBlock(format!(
                "base block gas mismatch: declared {}, executed {}",
                v1.gas_used,
                ctx.gas_used()
            )));
        }

        // Merged revenue is measured as the beneficiary balance delta from
        // this point on (after the base replay, matching the simulator).
        let initial_beneficiary_balance = balance_of(&mut ctx.vm, beneficiary)
            .map_err(|e| MergeError::Internal(e.to_string()))?;

        Ok(Self {
            base_block_hash: base.block_hash,
            base_builder_pubkey: base.builder_pubkey,
            beneficiary,
            beneficiary_alloy,
            base_value: base.block_value,
            builder_safe,
            ctx,
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
            max_tx_gas_limit,
            chain_id,
            best_emitted: U256::ZERO,
            last_emit: None,
            pending_emission: false,
            stats: MergeStats::default(),
            trace: MergeTraceV1 { base_block_recv_ns: base.recv_ns, ..Default::default() },
        })
    }

    /// Presimulates candidate orders in parallel, then greedily applies them
    /// best-payment-first to the live context. Returns whether the block
    /// changed. Port of `append_greedily_until_gas_limit`.
    pub fn try_extend(&mut self, orders: &[PreparedOrder]) -> bool {
        self.trace.sim_start_ns = utcnow_ns();
        let header = self.ctx.payload.header.clone();

        let candidates: Vec<usize> = (0..orders.len())
            .filter(|&ix| {
                let order = &orders[ix];
                !self.applied_orders.contains(&order.order_id) &&
                    order.source_block_hash != self.base_block_hash &&
                    simulate::gate_order(
                        order,
                        &self.tx_hashes,
                        self.available_gas(),
                        self.available_blobs(),
                    )
                    .is_ok()
            })
            .collect();
        if candidates.is_empty() {
            return false;
        }

        // Parallel presim on Evm clones (clones copy only touched accounts).
        self.stats.candidates_screened += candidates.len() as u64;
        let available_gas = self.available_gas();
        let available_blobs = self.available_blobs();
        let vm = &self.ctx.vm;
        let beneficiary = self.beneficiary;
        let results: Vec<Result<SimulatedOrder, SimulationError>> = {
            use rayon::prelude::*;
            candidates
                .par_iter()
                .map(|&ix| {
                    let mut vm = vm.clone();
                    simulate::simulate_order(
                        &mut vm,
                        &header,
                        &orders[ix],
                        ix,
                        available_gas,
                        available_blobs,
                        beneficiary,
                    )
                })
                .collect()
        };
        let mut simulated: Vec<SimulatedOrder> = Vec::with_capacity(results.len());
        for (result, &ix) in results.into_iter().zip(&candidates) {
            match result {
                Ok(order) => simulated.push(order),
                Err(err) => {
                    self.stats.count_sim_error(&err);
                    debug!(order = %orders[ix].order_id, %err, "order presim discarded");
                }
            }
        }

        // Highest payment first.
        simulated.sort_unstable_by(|a, b| b.builder_payment.cmp(&a.builder_payment));

        let mut changed = false;
        for candidate in simulated {
            let order = &orders[candidate.order_ix];
            match self.try_apply(order, &header) {
                Ok(true) => changed = true,
                Ok(false) => {}
                Err(err) => {
                    debug!(order = %order.order_id, %err, "order apply skipped");
                }
            }
        }

        self.trace.sim_end_ns = utcnow_ns();
        changed
    }

    /// Re-simulates `order` against the live state and, when still profitable,
    /// applies it for real. Rolls the context back on any violation.
    fn try_apply(
        &mut self,
        order: &PreparedOrder,
        header: &ethrex_common::types::BlockHeader,
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

        // Re-sim on the current state: earlier appends may have invalidated it.
        let mut sim_vm = self.ctx.vm.clone();
        let simulated = match simulate::simulate_order(
            &mut sim_vm,
            header,
            order,
            0,
            self.available_gas(),
            self.available_blobs(),
            self.beneficiary,
        ) {
            Ok(simulated) => simulated,
            Err(_) => return Ok(false),
        };

        // Snapshot for rollback.
        let vm_snapshot = self.ctx.vm.db.clone();
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
            if !simulated.include_tx[i] {
                continue;
            }
            let head = HeadTransaction {
                tx: ethrex_common::types::MempoolTransaction::new(
                    decoded.tx.clone(),
                    decoded.sender,
                ),
                tip: decoded.tx.effective_gas_tip(base_fee).unwrap_or_default(),
            };
            match self.blockchain.apply_tx_to_payload(head, &mut self.ctx) {
                Ok(()) => {
                    let succeeded = self.ctx.receipts.last().map(|r| r.succeeded).unwrap_or(false);
                    if !succeeded && !order.can_revert(i) {
                        rollback = true;
                        break;
                    }
                    applied_hashes.push(decoded.hash);
                    self.blob_count += decoded.blob_hashes.len() as u64;
                    self.appended_blobs.extend(decoded.blob_hashes.iter().copied());
                }
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

        if rollback || applied_hashes.is_empty() {
            if rollback {
                self.stats.apply_rollbacks += 1;
            }
            self.ctx.vm.db = vm_snapshot;
            let (remaining, cumulative, value, size, blob_gas, tx_len, receipts_len) =
                scalar_snapshot;
            self.ctx.remaining_gas = remaining;
            self.ctx.cumulative_gas_spent = cumulative;
            self.ctx.block_value = value;
            self.ctx.payload_size = size;
            self.ctx.payload.header.blob_gas_used = blob_gas;
            self.ctx.payload.body.transactions.truncate(tx_len);
            self.ctx.receipts.truncate(receipts_len);
            self.blob_count = blob_snapshot.0;
            self.appended_blobs.truncate(blob_snapshot.1);
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
        entry.revenue += simulated.builder_payment;
        entry.txs.extend(applied_hashes);
        Ok(true)
    }

    /// Builds the distribution tx on a clone of the live context, finalizes it
    /// and assembles the `MergedBlockV1`. Distinguishes "nothing to emit" from
    /// "improvement blocked by the spacing gate" so the worker can retry the
    /// latter.
    pub fn emit(
        &mut self,
        slot_number: u64,
        proposer_fee_recipient: Address,
        relay_config: &RelayConfigV1,
        engine_config: &EngineConfig,
    ) -> Result<EmitOutcome, MergeError> {
        self.pending_emission = false;
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

        // Emission gates: strict improvement, then spacing. Order matters:
        // hitting the spacing gate implies an improvement is waiting.
        if proposer_value <= self.best_emitted + engine_config.min_value_increase_wei {
            self.stats.emit_not_improved += 1;
            return Ok(EmitOutcome::NotImproved);
        }
        if let Some(last) = self.last_emit &&
            last.elapsed() < engine_config.min_emission_interval
        {
            self.stats.emit_throttled += 1;
            self.pending_emission = true;
            return Ok(EmitOutcome::Throttled);
        }

        // Finalization clears the vm caches, so it runs on a clone; the live
        // session stays extendable.
        let mut ctx = self.ctx.clone();

        let payment_gas_limit =
            self.max_tx_gas_limit.min(ctx.payload.header.gas_limit.saturating_sub(ctx.gas_used()));
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
            .copied();
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
            gas_limit: payment_gas_limit,
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

        self.blockchain
            .extract_requests(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("extract requests: {e}")))?;
        self.blockchain
            .apply_withdrawals(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("apply withdrawals: {e}")))?;
        self.blockchain
            .finalize_payload(&mut ctx)
            .map_err(|e| MergeError::Internal(format!("finalize payload: {e}")))?;

        self.trace.finalize_ns = utcnow_ns();

        let execution_payload = block_to_payload_v3(&ctx.payload);
        let execution_requests = requests_to_v4(ctx.requests.as_deref().unwrap_or_default())
            .map_err(|e| MergeError::Internal(format!("execution requests: {e}")))?;

        let builder_inclusions = self
            .revenues
            .iter()
            .map(|(origin, revenue)| BuilderInclusion {
                builder_pubkey: revenue.pubkey,
                origin_coinbase: *origin,
                revenue: revenue.revenue,
                txs: revenue.txs.clone(),
            })
            .collect();

        self.best_emitted = proposer_value;
        self.last_emit = Some(Instant::now());
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
            builder_inclusions,
            included_order_ids: self.included_order_ids.clone(),
            trace: self.trace,
        })))
    }

    fn available_gas(&self) -> u64 {
        self.gas_soft_limit.saturating_sub(self.ctx.gas_used())
    }

    fn available_blobs(&self) -> u64 {
        self.max_blobs.saturating_sub(self.blob_count)
    }

    pub fn has_pending_revenue(&self) -> bool {
        !self.revenues.is_empty()
    }

    #[cfg(test)]
    pub fn stats(&self) -> &MergeStats {
        &self.stats
    }

    /// One structured summary line, emitted when the session is finally
    /// discarded (slot end, connection reset, parked-eviction).
    pub fn log_stats(&self, reason: &str) {
        info!(
            reason,
            base_block_hash = %self.base_block_hash,
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
            emit_throttled = self.stats.emit_throttled,
            "merge session stats"
        );
    }
}
