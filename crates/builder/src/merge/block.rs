use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::{payload::calc_gas_limit, vm::StoreVmDatabase};
use ethrex_common::{
    Bloom, H256,
    constants::GAS_PER_BLOB,
    types::{
        BlockHeader, ChainConfig, ELASTICITY_MULTIPLIER, Receipt, Transaction, TxKind,
        bloom_from_logs, calc_excess_blob_gas, calculate_base_fee_per_gas,
        compute_withdrawals_root,
        requests::{EncodedRequests, compute_requests_hash},
    },
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::db::{CachingDatabase, Database, gen_db::TxReads};
use ethrex_storage::Store;
use ethrex_vm::DynVmDatabase;
use helix_tcp_types::merging::{
    builder_to_relay::{BuilderInclusion, MergeTraceV1, MergedBlockV1, UnmergedReason, UnmergedTx},
    control::RelayConfigV1,
};
use rustc_hash::FxHashSet;

use crate::{
    convert::{aaddr, au256, b256, eaddr, h256, payload_v3, requests_to_v4},
    merge::{
        effects::{EffectCache, NO_EFFECT, put_receipt},
        lists::ListRoot,
        payment::{self, Revenue, SafePayment},
        pool::{BaseBlock, Order, Pool, Tx},
        record::SlotRecorder,
        root::StateTries,
        state::{BlockState, Checkpoint, EngineVm, KeyTable},
        trie::NodeArena,
    },
    metrics,
    utils::utcnow_ns,
};

const SAFE_NONCE_SLOT: u64 = 5;

pub struct ParentState {
    pub header: BlockHeader,
    pub db: Arc<dyn Database>,
    pub chain: ChainConfig,
}

impl ParentState {
    pub fn open(
        store: &Store,
        hash: B256,
        recorder: Option<&Arc<SlotRecorder>>,
    ) -> Result<Self, String> {
        let header = store
            .get_block_header_by_hash(h256(hash))
            .map_err(|e| e.to_string())?
            .ok_or("parent not synced")?;
        let vm_db =
            StoreVmDatabase::new(store.clone(), header.clone()).map_err(|e| e.to_string())?;
        let mut db: Arc<dyn Database> = Arc::new(Box::new(vm_db) as DynVmDatabase);
        if let Some(recorder) = recorder {
            let _unused = recorder.parent_header.set(header.clone());
            let _unused = recorder.store.set(store.clone());
            db = recorder.wrap_db(db);
        }
        let db: Arc<dyn Database> = Arc::new(CachingDatabase::new(db, true));
        Ok(ParentState { header, db, chain: store.get_chain_config() })
    }
}

pub struct SlotCtx<'a> {
    pub store: &'a Store,
    pub parent: &'a ParentState,
    pub relay: &'a RelayConfigV1,
    pub slot: u64,
    pub proposer: Address,
    pub beacon_root: B256,
    pub blacklist: Option<&'a FxHashSet<Address>>,
    pub signer: &'a PrivateKeySigner,
    pub min_increase: U256,
}

pub struct BlockMerger {
    pub base: BaseBlock,
    pub live: bool,
    state: BlockState,
    coinbase_id: u32,
    collateral_safe: Address,
    order_gas_cap: u64,
    max_blobs: u64,
    blobs_used: u64,
    gas_used: u64,
    appended_txs: Vec<u32>,
    receipt_refs: Vec<(u32, u64)>,
    unrecorded: Vec<(Receipt, Bloom)>,
    in_block: Vec<u64>,
    order_marks: Vec<OrderMark>,
    effects: EffectCache,
    included_orders: Vec<(u32, u32)>,
    carried_orders: Vec<u32>,
    revenues: Vec<Revenue>,
    state_tries: StateTries,
    list_root: ListRoot,
    receipts_rlp: Vec<u8>,
    receipt_ends: Vec<usize>,
    pub seen_version: u64,
    pub best_value: U256,
    pub emissions: u32,
    trace: MergeTraceV1,
}

pub struct MergeScratch {
    solo_runs: Vec<(u32, U256)>,
    tx_reads: TxReads,
    deposit_receipts: Vec<Receipt>,
    vm: EngineVm,
}

impl Default for MergeScratch {
    fn default() -> MergeScratch {
        MergeScratch {
            solo_runs: Vec::with_capacity(1024),
            tx_reads: TxReads::default(),
            deposit_receipts: Vec::with_capacity(16),
            vm: EngineVm::default(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum OrderMark {
    #[default]
    Open,
    Taken,
    Unmerged(UnmergedReason),
}

impl BlockMerger {
    pub fn new() -> BlockMerger {
        BlockMerger {
            base: BaseBlock::default(),
            live: false,
            state: BlockState::default(),
            coinbase_id: 0,
            collateral_safe: Address::ZERO,
            order_gas_cap: 0,
            max_blobs: 0,
            blobs_used: 0,
            gas_used: 0,
            appended_txs: Vec::with_capacity(256),
            receipt_refs: Vec::with_capacity(1024),
            unrecorded: Vec::with_capacity(1024),
            in_block: Vec::with_capacity(64),
            order_marks: Vec::with_capacity(1024),
            effects: EffectCache::default(),
            included_orders: Vec::with_capacity(256),
            carried_orders: Vec::with_capacity(256),
            revenues: Vec::with_capacity(16),
            state_tries: StateTries::default(),
            list_root: ListRoot::default(),
            receipts_rlp: Vec::with_capacity(1 << 20),
            receipt_ends: Vec::with_capacity(1024),
            seen_version: u64::MAX,
            best_value: U256::ZERO,
            emissions: 0,
            trace: MergeTraceV1::default(),
        }
    }

    // Drops what refers to the ended slot's pool indices, key ids and parent; keeps buffers.
    pub fn reset(&mut self) {
        self.base.hash = B256::ZERO;
        self.live = false;
        self.included_orders.clear();
        self.effects.clear();
        self.state_tries.close();
    }

    pub fn open_base(
        &mut self,
        base: &BaseBlock,
        pool: &Pool,
        cx: &SlotCtx,
        keys: &mut KeyTable,
        scratch: &mut MergeScratch,
    ) -> Result<(), &'static str> {
        let header = &base.header;
        let parent = &cx.parent.header;
        let chain = &cx.parent.chain;
        let safe = cx.relay.collateral_safe(&aaddr(header.coinbase)).ok_or("unknown_collateral")?;
        if base.txs.len == 0 {
            return Err("no_payment_tx");
        }
        let reserve = cx.relay.distribution_gas_limit;
        if header.gas_limit.saturating_sub(header.gas_used) <= reserve {
            return Err("no_distribution_headroom");
        }
        if chain.is_amsterdam_activated(header.timestamp) {
            return Err("amsterdam");
        }
        let schedule = chain.get_fork_blob_schedule(header.timestamp);
        let max_blobs = schedule.map(|s| s.max as u64).unwrap_or_default();
        let txs = pool.txs_of(base.txs);
        let blobs: u64 = txs.iter().map(|&tx| pool.tx_fit(tx).1).sum();
        if blobs > max_blobs {
            return Err("too_many_blobs");
        }
        if header.number != parent.number + 1 ||
            calc_gas_limit(parent.gas_limit, header.gas_limit) != header.gas_limit
        {
            return Err("header_mismatch");
        }
        let base_fee = calculate_base_fee_per_gas(
            header.gas_limit,
            parent.gas_limit,
            parent.gas_used,
            parent.base_fee_per_gas.unwrap_or_default(),
            ELASTICITY_MULTIPLIER,
        );
        let excess = schedule
            .map(|s| calc_excess_blob_gas(parent, s, chain.fork(header.timestamp)))
            .unwrap_or_default();
        if base_fee.is_none() ||
            header.base_fee_per_gas != base_fee ||
            header.excess_blob_gas != Some(excess)
        {
            return Err("header_mismatch");
        }
        let coinbase = header.coinbase;
        let proposer = eaddr(cx.proposer);
        if proposer == coinbase {
            return Err("proposer_is_coinbase");
        }
        let base_gas_used = header.gas_used;

        self.live = false;
        self.base.copy_from(base);
        let header = &mut self.base.header;
        header.withdrawals_root = Some(compute_withdrawals_root(&base.withdrawals, &NativeCrypto));
        header.blob_gas_used = Some(0);
        header.parent_beacon_block_root = Some(h256(cx.beacon_root));
        let gas_limit = self.base.header.gas_limit;
        self.state.reset();
        self.coinbase_id = keys.intern_account(coinbase)?;
        let proposer = keys.intern_account(proposer)?;
        let header = &self.base.header;
        let ran = scratch.vm.with_state(keys, &self.state, |evm| evm.apply_system_calls(header));
        ran.map_err(|_| "system_calls")?;
        self.state.absorb_run(keys, &scratch.vm.evm.db)?;
        self.state.clear_undo();

        self.effects.reset_for(&self.base.header);
        self.receipt_refs.clear();
        self.unrecorded.clear();
        self.gas_used = 0;
        let mut paid_before = ethrex_common::U256::zero();
        let last = txs.len() - 1;
        for (i, &ix) in txs.iter().enumerate() {
            let tx = pool.tx(ix);
            if cx.blacklist.is_some_and(|list| calls_blacklisted(list, tx)) {
                return Err("blacklisted");
            }
            if pool.tx_fit(ix).0 > gas_limit.saturating_sub(self.gas_used) {
                return Err("gas_allowance");
            }
            if i == last {
                paid_before = self.state.account(keys, proposer).balance;
            }
            let at = self.state.checkpoint();
            self.apply_tx(ix, pool, keys, scratch).ok_or("base_tx_invalid")?;
            let coinbase = self.coinbase_id;
            if cx
                .blacklist
                .is_some_and(|list| touches_blacklisted(list, keys, &self.state, at, coinbase))
            {
                return Err("blacklisted");
            }
            self.state.clear_undo();
        }
        if self.gas_used != base_gas_used {
            return Err("gas_used_mismatch");
        }
        // The proposer's balance delta across the last tx proves the bid.
        let paid = self.state.account(keys, proposer).balance.saturating_sub(paid_before);
        if au256(paid) < pool.block_values[self.base.block as usize] {
            return Err("invalid_payment");
        }
        let header = &self.base.header;

        self.in_block.clear();
        self.in_block.resize(pool.txs.len().div_ceil(64), 0);
        for &tx in txs {
            self.in_block[tx as usize / 64] |= 1 << (tx % 64);
        }
        self.collateral_safe = safe;
        self.order_gas_cap = header.gas_limit - reserve;
        self.max_blobs = max_blobs;
        self.blobs_used = blobs;
        self.appended_txs.clear();
        self.order_marks.clear();
        self.carried_orders.clear();
        self.carried_orders.extend(self.included_orders.iter().map(|&(ix, _)| ix));
        self.included_orders.clear();
        self.revenues.clear();
        self.seen_version = u64::MAX;
        self.best_value = U256::ZERO;
        self.emissions = 0;
        self.trace = MergeTraceV1 { base_block_recv_ns: self.base.recv_ns, ..Default::default() };
        self.live = true;
        Ok(())
    }

    pub fn extend(
        &mut self,
        pool: &Pool,
        cx: &SlotCtx,
        keys: &mut KeyTable,
        scratch: &mut MergeScratch,
    ) {
        self.trace.sim_start_ns = utcnow_ns();
        self.seen_version = pool.orders_version;
        self.order_marks.resize(pool.orders.len(), OrderMark::Open);
        self.in_block.resize(pool.txs.len().div_ceil(64), 0);
        let carried = std::mem::take(&mut self.carried_orders);
        for &ix in &carried {
            let order = &pool.orders[ix as usize];
            if order.withdrawn || order.best_block == self.base.block {
                continue;
            }
            let ran = self
                .can_fit(order, pool)
                .and_then(|()| self.run_order(order, pool, cx, true, keys, scratch));
            match ran {
                Ok((paid, kept)) => self.commit_order(ix, order, pool, paid, kept),
                Err(reason) => self.order_marks[ix as usize] = OrderMark::Unmerged(reason),
            }
        }
        self.carried_orders = carried;
        self.carried_orders.clear();
        let mut runs = std::mem::take(&mut scratch.solo_runs);
        runs.clear();
        for (ix, order) in pool.orders.iter().enumerate() {
            if self.order_marks[ix] == OrderMark::Taken || order.best_block == self.base.block {
                continue;
            }
            if order.withdrawn {
                self.order_marks[ix] = OrderMark::Unmerged(UnmergedReason::Replaced);
                continue;
            }
            let ran = self
                .can_fit(order, pool)
                .and_then(|()| self.run_order(order, pool, cx, false, keys, scratch));
            match ran {
                Ok((paid, _)) => runs.push((ix as u32, paid)),
                Err(reason) => self.order_marks[ix] = OrderMark::Unmerged(reason),
            }
        }
        runs.sort_by_key(|r| std::cmp::Reverse(r.1));
        for &(ix, _) in runs.iter() {
            let order = &pool.orders[ix as usize];
            let ran = self
                .can_fit(order, pool)
                .and_then(|()| self.run_order(order, pool, cx, true, keys, scratch));
            match ran {
                Ok((paid, kept)) => self.commit_order(ix, order, pool, paid, kept),
                Err(reason) => self.order_marks[ix as usize] = OrderMark::Unmerged(reason),
            }
        }
        scratch.solo_runs = runs;
        self.trace.sim_end_ns = utcnow_ns();
    }

    fn can_fit(&self, order: &Order, pool: &Pool) -> Result<(), UnmergedReason> {
        let (mut gas, mut blobs) = (0, 0);
        for (i, &tx) in pool.txs_of(order.txs).iter().enumerate() {
            if order.may_drop & (1 << i) != 0 {
                continue;
            }
            if self.in_block[tx as usize / 64] & (1 << (tx % 64)) != 0 {
                return Err(UnmergedReason::Duplicate);
            }
            let (tx_gas, tx_blobs) = pool.tx_fit(tx);
            gas += tx_gas;
            blobs += tx_blobs;
        }
        if gas > self.order_gas_cap - self.gas_used || blobs > self.max_blobs - self.blobs_used {
            return Err(UnmergedReason::OutOfSpace);
        }
        Ok(())
    }

    fn run_order(
        &mut self,
        order: &Order,
        pool: &Pool,
        cx: &SlotCtx,
        keep: bool,
        keys: &mut KeyTable,
        scratch: &mut MergeScratch,
    ) -> Result<(U256, u32), UnmergedReason> {
        let txs = pool.txs_of(order.txs);
        if cx
            .blacklist
            .is_some_and(|list| txs.iter().any(|&tx| calls_blacklisted(list, pool.tx(tx))))
        {
            return Err(UnmergedReason::Invalid);
        }
        let coinbase = self.coinbase_id;
        let paid_before = self.state.account(keys, coinbase).balance;
        let gas_left = self.order_gas_cap - self.gas_used;
        let blobs_left = self.max_blobs - self.blobs_used;
        let (cumulative, receipts, start) =
            (self.gas_used, self.receipt_refs.len(), self.state.checkpoint());
        let (mut gas, mut blobs, mut kept) = (0, 0, 0u32);
        let mut failed = None;
        for (i, &ix) in txs.iter().enumerate() {
            let (tx_gas_limit, tx_blobs) = pool.tx_fit(ix);
            let bit = 1u32 << i;
            let (can_drop, can_revert) = (order.may_drop & bit != 0, order.may_revert & bit != 0);
            if tx_gas_limit > gas_left - gas || tx_blobs > blobs_left - blobs {
                if can_drop {
                    continue;
                }
                failed = Some(UnmergedReason::OutOfSpace);
                break;
            }
            let (at, mark) = (self.gas_used, self.state.checkpoint());
            let Some((tx_gas, succeeded)) = self.apply_tx(ix, pool, keys, scratch) else {
                self.gas_used = at;
                if can_drop || can_revert {
                    continue;
                }
                failed = Some(UnmergedReason::Invalid);
                break;
            };
            let listed = cx
                .blacklist
                .is_some_and(|list| touches_blacklisted(list, keys, &self.state, mark, coinbase));
            let reason = if listed {
                Some((UnmergedReason::Invalid, false))
            } else if !succeeded && !can_revert {
                Some((UnmergedReason::Invalid, can_drop))
            } else if gas + tx_gas > gas_left {
                Some((UnmergedReason::OutOfSpace, can_drop))
            } else {
                None
            };
            if let Some((reason, droppable)) = reason {
                self.state.undo_to(mark);
                self.truncate_receipts(self.receipt_refs.len() - 1);
                self.gas_used = at;
                if droppable {
                    continue;
                }
                failed = Some(reason);
                break;
            }
            gas += tx_gas;
            blobs += tx_blobs;
            kept |= bit;
        }
        let paid = au256(self.state.account(keys, coinbase).balance.saturating_sub(paid_before));
        if failed.is_none() && (kept == 0 || paid.is_zero()) {
            failed = Some(UnmergedReason::ZeroPayment);
        }
        if !keep || failed.is_some() {
            self.state.undo_to(start);
            self.truncate_receipts(receipts);
            self.gas_used = cumulative;
        } else {
            self.state.clear_undo();
        }
        if let Some(reason) = failed {
            return Err(reason);
        }
        if keep {
            self.blobs_used += blobs;
        }
        Ok((paid, kept))
    }

    fn truncate_receipts(&mut self, len: usize) {
        let own = self.receipt_refs[len..].iter().filter(|r| r.0 == NO_EFFECT).count();
        self.unrecorded.truncate(self.unrecorded.len() - own);
        self.receipt_refs.truncate(len);
    }

    fn apply_tx(
        &mut self,
        ix: u32,
        pool: &Pool,
        keys: &mut KeyTable,
        scratch: &mut MergeScratch,
    ) -> Option<(u64, bool)> {
        let (at, cumulative) = (self.state.checkpoint(), self.gas_used);
        let (vm, reads) = (&mut scratch.vm, &mut scratch.tx_reads);
        if let Some(effect) = self.effects.find_replayable(ix, keys, &self.state, self.coinbase_id)
        {
            let Some(gas) = self.effects.replay(effect, keys, &mut self.state, self.coinbase_id)
            else {
                self.state.undo_to(at);
                return None;
            };
            #[cfg(debug_assertions)]
            self.check_replay(ix, pool, keys, vm, at, gas);
            self.gas_used += gas;
            self.receipt_refs.push((effect, self.gas_used));
            return Some((gas, self.effects.receipt(effect).0.succeeded));
        }
        let tx = pool.tx(ix);
        let before = self.state.account(keys, self.coinbase_id).balance;
        reads.accounts.clear();
        reads.slots.clear();
        reads.balances.clear();
        reads.floors.clear();
        reads.coinbase_address = false;
        let (header, gas_used) = (&self.base.header, &mut self.gas_used);
        let ran = vm.with_state(keys, &self.state, |evm| {
            evm.db.tx_reads = Some(std::mem::take(reads));
            let ran = evm.execute_tx(&tx.tx, header, gas_used, tx.sender);
            *reads = evm.db.tx_reads.take().unwrap_or_default();
            ran
        });
        let (receipt, report) = ran.ok()?;
        if let Err(reason) = self.state.absorb_run(keys, &vm.evm.db) {
            metrics::rejection("merge_state", reason);
            self.state.undo_to(at);
            self.gas_used = cumulative;
            return None;
        }
        let (gas, succeeded) = (report.gas_spent, receipt.succeeded);
        let (state, own) = (&self.state, &mut self.unrecorded);
        let effect = self.effects.record(
            ix,
            gas,
            keys,
            state,
            at,
            reads,
            self.coinbase_id,
            before,
            receipt,
            own,
        );
        self.receipt_refs.push((effect, self.gas_used));
        Some((gas, succeeded))
    }

    #[cfg(debug_assertions)]
    fn check_replay(
        &mut self,
        ix: u32,
        pool: &Pool,
        keys: &mut KeyTable,
        vm: &mut EngineVm,
        at: Checkpoint,
        gas: u64,
    ) {
        let state = &self.state;
        let mut replayed: Vec<_> =
            state.accounts_since(at).map(|id| (id, state.account(keys, id))).collect();
        let mut replayed_slots: Vec<_> =
            state.slots_since(at).map(|id| (id, state.slot(keys, id))).collect();
        self.state.undo_to(at);
        replayed.retain(|&(id, v)| self.state.account(keys, id) != v);
        replayed_slots.retain(|&(id, v)| self.state.slot(keys, id) != v);
        let tx = pool.tx(ix);
        let mut cumulative = 0;
        let header = &self.base.header;
        let ran = vm.with_state(keys, &self.state, |evm| {
            evm.execute_tx(&tx.tx, header, &mut cumulative, tx.sender)
        });
        let (_, report) = ran.expect("replayed tx fails when run");
        self.state.absorb_run(keys, &vm.evm.db).expect("replayed tx does not fold");
        let state = &self.state;
        let mut run: Vec<_> =
            state.accounts_since(at).map(|id| (id, state.account(keys, id))).collect();
        let mut run_slots: Vec<_> =
            state.slots_since(at).map(|id| (id, state.slot(keys, id))).collect();
        for list in [&mut replayed, &mut run] {
            list.sort_unstable_by_key(|w| w.0);
        }
        for list in [&mut replayed_slots, &mut run_slots] {
            list.sort_unstable_by_key(|w| w.0);
        }
        assert_eq!(report.gas_spent, gas, "replayed gas, tx {ix}");
        assert_eq!(replayed, run, "replayed account writes, tx {ix}");
        assert_eq!(replayed_slots, run_slots, "replayed slot writes, tx {ix}");
    }

    fn commit_order(&mut self, ix: u32, order: &Order, pool: &Pool, paid: U256, kept: u32) {
        for (i, &tx) in pool.txs_of(order.txs).iter().enumerate() {
            if kept & (1 << i) == 0 {
                continue;
            }
            self.appended_txs.push(tx);
            self.in_block[tx as usize / 64] |= 1 << (tx % 64);
        }
        self.order_marks[ix as usize] = OrderMark::Taken;
        self.included_orders.push((ix, kept));
        let origin = pool.submitters[order.submitter as usize].address;
        let same = |r: &&mut Revenue| pool.submitters[r.submitter as usize].address == origin;
        match self.revenues.iter_mut().find(same) {
            Some(revenue) => revenue.paid += paid,
            None => self.revenues.push(Revenue { submitter: order.submitter, paid }),
        }
    }

    pub fn emit(
        &mut self,
        pool: &Pool,
        cx: &SlotCtx,
        keys: &mut KeyTable,
        nodes: &mut NodeArena,
        scratch: &mut MergeScratch,
    ) -> Result<Option<MergedBlockV1>, &'static str> {
        let total: U256 = self.revenues.iter().map(|r| r.paid).sum();
        let base_fee = self.base.header.base_fee_per_gas.unwrap_or_default();
        let cost = U256::from(base_fee) * U256::from(cx.relay.distribution_gas_limit);
        if total <= cost {
            return Ok(None);
        }
        let coinbase = aaddr(self.base.header.coinbase);
        let gas_payer = cx.signer.address();
        let to = payment::split_revenue(
            cx.relay,
            &self.revenues,
            &pool.submitters,
            cost,
            gas_payer,
            cx.proposer,
            coinbase,
        );
        let share = |address: Address| {
            to.iter().find(|(a, _)| *a == address).map(|(_, v)| *v).unwrap_or_default()
        };
        let proposer_value = pool.block_values[self.base.block as usize] + share(cx.proposer);
        let paid_out: U256 = to.iter().map(|(_, v)| *v).sum();
        let builder_revenue = total.saturating_sub(paid_out);
        if builder_revenue.is_zero() || proposer_value <= self.best_value + cx.min_increase {
            return Ok(None);
        }

        let safe = eaddr(self.collateral_safe);
        let signer = eaddr(cx.signer.address());
        let (safe_id, signer_id) = (keys.intern_account(safe)?, keys.intern_account(signer)?);
        let nonce_id = keys.intern_slot(safe, H256::from_low_u64_be(SAFE_NONCE_SLOT))?;
        let p = SafePayment {
            safe: self.collateral_safe,
            safe_balance: au256(self.state.account(keys, safe_id).balance),
            safe_nonce: self.state.slot(keys, nonce_id).low_u64(),
            signer_nonce: self.state.account(keys, signer_id).nonce,
            chain_id: cx.parent.chain.chain_id,
            gas_limit: cx.relay.distribution_gas_limit,
            base_fee: base_fee as u128,
            multisend: cx.relay.multisend_contract,
        };
        let encoded = payment::payment_tx(cx.signer, &p, &to)?;
        let payment = Transaction::decode_canonical(&encoded).map_err(|_| "payment_decode")?;

        let header = &self.base.header;
        // The distribution, requests and withdrawals are folded in only for the state root.
        let prague = cx.parent.chain.is_prague_activated(header.timestamp);
        let deposits = &mut scratch.deposit_receipts;
        deposits.clear();
        let deposit = cx.parent.chain.deposit_contract_address;
        for (receipt, ..) in block_receipts(&self.receipt_refs, &self.unrecorded, &self.effects) {
            if prague && receipt.logs.iter().any(|log| log.address == deposit) {
                deposits.push(receipt.clone());
            }
        }
        let withdrawals = &self.base.withdrawals;
        let mut cumulative = self.gas_used;
        let ran = scratch.vm.with_state(keys, &self.state, |evm| {
            let (receipt, _) = evm
                .execute_tx(&payment, header, &mut cumulative, signer)
                .map_err(|_| "payment_invalid")?;
            if !receipt.succeeded {
                return Err("payment_reverted");
            }
            deposits.push(receipt);
            let requests = match prague {
                true => evm.extract_requests(deposits, header).map_err(|_| "requests"),
                false => Ok(Vec::new()),
            };
            let receipt = deposits.pop().ok_or("requests")?;
            let requests: Vec<EncodedRequests> = requests?.iter().map(|r| r.encode()).collect();
            evm.process_withdrawals(withdrawals).map_err(|_| "withdrawals")?;
            Ok((receipt, requests))
        });
        let (receipt, requests) = ran?;
        let at = self.state.checkpoint();
        let folded = self.state.absorb_run(keys, &scratch.vm.evm.db);
        let state_root = folded.and_then(|()| {
            let (store, db) = (cx.store, cx.parent.db.as_ref());
            let parent_root = cx.parent.header.state_root;
            let tries = &mut self.state_tries;
            let reused = tries.is_open();
            if !reused {
                tries.open(store, parent_root, nodes)?;
            }
            let mut state_root = tries.update(store, db, nodes, keys, &self.state);
            // Rewriting from the last block can need parent nodes a fresh open does not.
            if state_root.is_err() && reused {
                metrics::rejection("state_root", "reopened");
                tries.open(store, parent_root, nodes)?;
                state_root = tries.update(store, db, nodes, keys, &self.state);
            }
            if state_root.is_err() {
                tries.close();
            }
            state_root
        });
        self.state.undo_to(at);
        let state_root = state_root?;

        let base = pool.txs_of(self.base.txs).iter();
        let mut txs = Vec::with_capacity(self.receipt_refs.len() + 1);
        txs.extend(base.chain(&self.appended_txs).map(|&tx| pool.tx(tx).encoded.clone()));
        txs.push(encoded.into());
        let paid = bloom_from_logs(&receipt.logs, &NativeCrypto);
        let paid = (&receipt, cumulative, &paid, None);
        let mut logs_bloom = Bloom::zero();
        self.receipts_rlp.clear();
        self.receipt_ends.clear();
        self.receipt_ends.push(0);
        let receipts = block_receipts(&self.receipt_refs, &self.unrecorded, &self.effects);
        for (receipt, gas, bloom, logs) in receipts.chain([paid]) {
            logs_bloom |= *bloom;
            match logs {
                Some(logs) => put_receipt(&mut self.receipts_rlp, receipt, gas, bloom, logs),
                None => {
                    let encoded = receipt.encode_inner_with_precomputed_bloom(*bloom);
                    self.receipts_rlp.extend_from_slice(&encoded);
                }
            }
            self.receipt_ends.push(self.receipts_rlp.len());
        }
        let (bytes, at) = (&self.receipts_rlp, &self.receipt_ends);
        let mut header = self.base.header.clone();
        header.state_root = state_root;
        header.transactions_root = self.list_root.compute(txs.len(), |i| &txs[i]);
        header.receipts_root = self.list_root.compute(at.len() - 1, |i| &bytes[at[i]..at[i + 1]]);
        header.logs_bloom = logs_bloom;
        header.gas_used = cumulative;
        header.blob_gas_used = Some(self.blobs_used * u64::from(GAS_PER_BLOB));
        if prague {
            header.requests_hash = Some(compute_requests_hash(&requests));
        }
        let execution_payload = payload_v3(&header, txs, &self.base.withdrawals);

        let inclusions = self
            .revenues
            .iter()
            .map(|r| {
                let submitter = &pool.submitters[r.submitter as usize];
                BuilderInclusion {
                    builder_pubkey: submitter.pubkey,
                    origin_coinbase: submitter.address,
                    contribution: r.paid,
                    revenue: share(submitter.address),
                    txs: self
                        .included_orders
                        .iter()
                        .filter(|&&(ix, _)| {
                            let order = &pool.orders[ix as usize];
                            pool.submitters[order.submitter as usize].address == submitter.address
                        })
                        .flat_map(|&(ix, kept)| {
                            let txs = pool.txs_of(pool.orders[ix as usize].txs).iter().enumerate();
                            txs.filter(move |&(i, _)| kept & (1 << i) != 0)
                                .map(|(_, &tx)| pool.tx(tx).hash)
                        })
                        .collect(),
                }
            })
            .collect();
        let unmerged = self.order_marks.iter().zip(&pool.orders).flat_map(|(&mark, order)| {
            let reason = match mark {
                OrderMark::Unmerged(reason) => Some(reason),
                OrderMark::Open | OrderMark::Taken => None,
            };
            reason.into_iter().flat_map(|reason| {
                pool.txs_of(order.txs)
                    .iter()
                    .map(move |&tx| UnmergedTx { tx_hash: pool.tx(tx).hash, reason })
            })
        });
        self.trace.finalize_ns = utcnow_ns();
        self.best_value = proposer_value;
        self.emissions += 1;
        Ok(Some(MergedBlockV1 {
            slot: cx.slot,
            response_id: 0,
            base_block_hash: self.base.hash,
            base_builder_pubkey: pool.submitters[self.base.submitter as usize].pubkey,
            execution_payload,
            execution_requests: requests_to_v4(&requests).map_err(|_| "requests")?,
            appended_blobs: self
                .appended_txs
                .iter()
                .flat_map(|&tx| pool.tx(tx).blob_hashes().iter().map(|h| b256(*h)))
                .collect(),
            proposer_value,
            base_builder_revenue: builder_revenue,
            relay_revenue: share(cx.relay.relay_fee_recipient),
            builder_inclusions: inclusions,
            included_order_ids: self
                .included_orders
                .iter()
                .map(|&(ix, _)| pool.order_ids[ix as usize])
                .collect(),
            trace: self.trace,
            unmerged_txs: unmerged.collect(),
        }))
    }
}

fn block_receipts<'a>(
    receipts: &'a [(u32, u64)],
    own: &'a [(Receipt, Bloom)],
    effects: &'a EffectCache,
) -> impl Iterator<Item = (&'a Receipt, u64, &'a Bloom, Option<&'a [u8]>)> {
    let mut own = own.iter();
    receipts.iter().map(move |&(effect, gas)| match effect {
        NO_EFFECT => {
            let (receipt, bloom) = own.next().expect("an own receipt per NO_EFFECT");
            (receipt, gas, bloom, None)
        }
        _ => {
            let (receipt, bloom, logs) = effects.receipt(effect);
            (receipt, gas, bloom, Some(logs))
        }
    })
}

fn calls_blacklisted(list: &FxHashSet<Address>, tx: &Tx) -> bool {
    matches!(tx.tx.to(), TxKind::Call(to) if list.contains(&aaddr(to)))
}

fn touches_blacklisted(
    list: &FxHashSet<Address>,
    keys: &KeyTable,
    state: &BlockState,
    at: Checkpoint,
    coinbase: u32,
) -> bool {
    if list.is_empty() {
        return false;
    }
    let listed = |a: ethrex_common::Address| list.contains(&aaddr(a));
    state.accounts_since(at).any(|id| id != coinbase && listed(keys.parent_account(id).0)) ||
        state.slots_since(at).any(|id| listed(keys.parent_slot(id).0))
}
