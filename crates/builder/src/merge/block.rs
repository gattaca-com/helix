//! One base and the orders put on it, as a `State` over the slot's parent. Each tx replays its
//! effect or runs in the engine's EVM and is folded in. Orders run on top best payment first; a tx
//! or order that breaks its inclusion rules is undone from the state's undo log. An emission runs
//! the distribution, withdrawals and requests over the state, folds them in for the state root,
//! then undoes them. The state root is updated from the builder's last emitted block (`Root`); the
//! other roots are computed from scratch.

use std::{cell::RefCell, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use dashmap::DashSet;
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

use crate::{
    convert::{aaddr, au256, b256, eaddr, h256, payload_v3, requests_to_v4},
    merge::{
        effects::{Effects, NONE, put_receipt},
        ingest::{Base, Order, Pool, Tx},
        lists::Lists,
        payment::{self, Revenue, SafePayment},
        record::SlotRecorder,
        root::Root,
        state::{At, Keys, State, Vm},
        trie::Arena,
    },
    metrics,
    utils::utcnow_ns,
};

/// Safe storage slot holding its nonce.
const SAFE_NONCE_SLOT: u64 = 5;

/// The slot's parent: its header and its state, cached across every base of the slot.
pub struct Parent {
    pub header: BlockHeader,
    pub db: Arc<dyn Database>,
    pub chain: ChainConfig,
    /// Every key the slot's merges touched.
    pub keys: RefCell<Keys>,
}

impl Parent {
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
            db = recorder.wrap(db);
        }
        let db: Arc<dyn Database> = Arc::new(CachingDatabase::new(db, true));
        let keys = RefCell::new(Keys::new(db.clone()));
        Ok(Parent { header, db, chain: store.get_chain_config(), keys })
    }
}

/// Slot-fixed inputs to every merge.
pub struct Cx<'a> {
    pub store: &'a Store,
    pub parent: &'a Parent,
    /// Trie nodes every builder's state root has read, kept across parents.
    pub nodes: &'a RefCell<Arena>,
    pub relay: &'a RelayConfigV1,
    pub slot: u64,
    pub proposer: Address,
    pub beacon_root: B256,
    /// Set on slots the proposer filters.
    pub blacklist: Option<&'a DashSet<Address>>,
    pub signer: &'a PrivateKeySigner,
    pub min_increase: U256,
}

pub struct Merge {
    /// Its header is the merged block's, less what `seal` fills.
    pub base: Base,
    /// Holds a base that can still be extended and emitted.
    pub live: bool,
    /// State after the base and every committed order.
    state: State,
    /// The coinbase's account id.
    coinbase: u32,
    safe: Address,
    /// Block gas limit less the distribution's reserve.
    gas_cap: u64,
    max_blobs: u64,
    blobs: u64,
    /// Gas through the last tx: the receipts' cumulative gas and, before Amsterdam, the block's
    /// gas used.
    cumulative: u64,
    /// Pool txs after the base's.
    appended: Vec<u32>,
    /// By block tx, the base's then `appended`: the effect holding its receipt, or `NONE` for the
    /// next of `own`, and its cumulative gas.
    receipts: Vec<(u32, u64)>,
    /// Receipts of the block's txs not recorded as effects, in block order, with logs blooms.
    own: Vec<(Receipt, Bloom)>,
    /// Bit per pool tx: in the block.
    in_block: Vec<u64>,
    /// By pool order index.
    marks: Vec<Mark>,
    /// The builder's txs as effects, for its later bases and orders to replay.
    effects: Effects,
    /// Committed orders, each with its kept-tx bits.
    included: Vec<(u32, u32)>,
    /// The orders committed on the builder's previous base, in commit order: the first extend
    /// commits them before anything else.
    carried: Vec<u32>,
    revenues: Vec<Revenue>,
    /// Made at the first emission and kept across bases; dropped on any error.
    root: Option<Root>,
    lists: Lists,
    /// The block's encoded receipts, at `receipt_at[i]..receipt_at[i + 1]`.
    receipt_bytes: Vec<u8>,
    receipt_at: Vec<usize>,
    /// Pool version the last extend saw.
    pub seen: u64,
    pub best: U256,
    pub emissions: u32,
    trace: MergeTraceV1,
}

/// What every merge on the engine thread shares for the length of one call.
pub struct Scratch {
    /// Orders run alone by the current extend, with what each paid.
    runs: Vec<(u32, U256)>,
    /// What the tx being run reads.
    reads: TxReads,
    /// Receipts with deposit contract logs, for an emission's requests.
    deposits: Vec<Receipt>,
    vm: Vm,
}

impl Default for Scratch {
    fn default() -> Scratch {
        Scratch {
            runs: Vec::with_capacity(1024),
            reads: TxReads::default(),
            deposits: Vec::with_capacity(16),
            vm: Vm::default(),
        }
    }
}

/// Where a pool order stands on one base. An order not taken is retried on every extend.
#[derive(Clone, Copy, Default, PartialEq)]
enum Mark {
    #[default]
    Open,
    Taken,
    Unmerged(UnmergedReason),
}

impl Merge {
    /// A merge with no base on the slot's parent, its buffers sized for a typical block.
    pub fn new() -> Merge {
        Merge {
            base: Base::default(),
            live: false,
            state: State::default(),
            coinbase: 0,
            safe: Address::ZERO,
            gas_cap: 0,
            max_blobs: 0,
            blobs: 0,
            cumulative: 0,
            appended: Vec::with_capacity(256),
            receipts: Vec::with_capacity(1024),
            own: Vec::with_capacity(1024),
            in_block: Vec::with_capacity(64),
            marks: Vec::with_capacity(1024),
            effects: Effects::default(),
            included: Vec::with_capacity(256),
            carried: Vec::with_capacity(256),
            revenues: Vec::with_capacity(16),
            root: None,
            lists: Lists::default(),
            receipt_bytes: Vec::with_capacity(1 << 20),
            receipt_at: Vec::with_capacity(1024),
            seen: u64::MAX,
            best: U256::ZERO,
            emissions: 0,
            trace: MergeTraceV1::default(),
        }
    }

    /// Validates `base` against the parent and runs it, replacing the merge's base; buffers keep
    /// their capacity. Failures are reasons, not rejects: a base that cannot be merged is just not
    /// merged, and the merge is left not live.
    pub fn open(
        &mut self,
        mut base: Base,
        pool: &Pool,
        cx: &Cx,
        scratch: &mut Scratch,
    ) -> Result<(), &'static str> {
        self.live = false;
        let header = &mut base.header;
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
        let txs = pool.refs(base.txs);
        let blobs: u64 = txs.iter().map(|&tx| pool.tx(tx).blobs().len() as u64).sum();
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
        header.withdrawals_root = Some(compute_withdrawals_root(&base.withdrawals, &NativeCrypto));
        header.blob_gas_used = Some(0);
        header.parent_beacon_block_root = Some(h256(cx.beacon_root));
        let base_gas_used = header.gas_used;

        self.base = base;
        let gas_limit = self.base.header.gas_limit;
        let keys = &mut *cx.parent.keys.borrow_mut();
        self.state.reset();
        self.coinbase = keys.account(coinbase)?;
        let proposer = keys.account(proposer)?;
        let header = &self.base.header;
        let ran = scratch.vm.run(keys, &self.state, |evm| evm.apply_system_calls(header));
        ran.map_err(|_| "system_calls")?;
        self.state.fold(keys, &scratch.vm.evm.db)?;
        self.state.forget();

        self.effects.open(&self.base.header);
        self.receipts.clear();
        self.own.clear();
        self.cumulative = 0;
        let mut paid_before = ethrex_common::U256::zero();
        let last = txs.len() - 1;
        for (i, &ix) in txs.iter().enumerate() {
            let tx = pool.tx(ix);
            if cx.blacklist.is_some_and(|list| calls_listed(list, tx)) {
                return Err("blacklisted");
            }
            if tx.tx.gas_limit() > gas_limit.saturating_sub(self.cumulative) {
                return Err("gas_allowance");
            }
            if i == last {
                paid_before = self.state.account(keys, proposer).balance;
            }
            let at = self.state.at();
            self.step(ix, pool, keys, scratch).ok_or("base_tx_invalid")?;
            let coinbase = self.coinbase;
            if cx
                .blacklist
                .is_some_and(|list| touches_listed(list, keys, &self.state, at, coinbase))
            {
                return Err("blacklisted");
            }
            self.state.forget();
        }
        if self.cumulative != base_gas_used {
            return Err("gas_used_mismatch");
        }
        // The proposer's balance change across the last tx proves the bid: it covers a direct
        // transfer and a contract payment alike, and a reverted payment shows none.
        let paid = self.state.account(keys, proposer).balance.saturating_sub(paid_before);
        if au256(paid) < pool.values[self.base.block as usize] {
            return Err("invalid_payment");
        }
        let header = &self.base.header;

        self.in_block.clear();
        self.in_block.resize(pool.txs.len().div_ceil(64), 0);
        for &tx in txs {
            self.in_block[tx as usize / 64] |= 1 << (tx % 64);
        }
        self.safe = safe;
        self.gas_cap = header.gas_limit - reserve;
        self.max_blobs = max_blobs;
        self.blobs = blobs;
        self.appended.clear();
        self.marks.clear();
        self.carried.clear();
        self.carried.extend(self.included.iter().map(|&(ix, _)| ix));
        self.included.clear();
        self.revenues.clear();
        self.seen = u64::MAX;
        self.best = U256::ZERO;
        self.emissions = 0;
        self.trace = MergeTraceV1 { base_block_recv_ns: self.base.recv_ns, ..Default::default() };
        self.live = true;
        Ok(())
    }

    /// Puts every pool order not yet on this base on top of it: each is run alone on the current
    /// state, then they are committed best payment first, each re-run on what the earlier ones
    /// left.
    pub fn extend(&mut self, pool: &Pool, cx: &Cx, scratch: &mut Scratch) {
        self.trace.sim_start_ns = utcnow_ns();
        self.seen = pool.version;
        self.marks.resize(pool.orders.len(), Mark::Open);
        self.in_block.resize(pool.txs.len().div_ceil(64), 0);
        let keys = &mut *cx.parent.keys.borrow_mut();
        // Run once, on top of the ones before them as on the last base, so most txs replay. One
        // that fails is left to the alone runs below like any other.
        let carried = std::mem::take(&mut self.carried);
        for &ix in &carried {
            let order = &pool.orders[ix as usize];
            if order.withdrawn || order.block == self.base.block {
                continue;
            }
            let ran = self
                .fits(order, pool)
                .and_then(|()| self.run(order, pool, cx, true, keys, scratch));
            match ran {
                Ok((paid, kept)) => self.commit(ix, order, pool, paid, kept),
                Err(reason) => self.marks[ix as usize] = Mark::Unmerged(reason),
            }
        }
        self.carried = carried;
        self.carried.clear();
        let mut runs = std::mem::take(&mut scratch.runs);
        runs.clear();
        for (ix, order) in pool.orders.iter().enumerate() {
            if self.marks[ix] == Mark::Taken || order.block == self.base.block {
                continue;
            }
            if order.withdrawn {
                self.marks[ix] = Mark::Unmerged(UnmergedReason::Replaced);
                continue;
            }
            let ran = self
                .fits(order, pool)
                .and_then(|()| self.run(order, pool, cx, false, keys, scratch));
            match ran {
                Ok((paid, _)) => runs.push((ix as u32, paid)),
                Err(reason) => self.marks[ix] = Mark::Unmerged(reason),
            }
        }
        // Stable: equal payments keep pool order.
        runs.sort_by_key(|r| std::cmp::Reverse(r.1));
        for &(ix, _) in runs.iter() {
            let order = &pool.orders[ix as usize];
            let ran = self
                .fits(order, pool)
                .and_then(|()| self.run(order, pool, cx, true, keys, scratch));
            match ran {
                Ok((paid, kept)) => self.commit(ix, order, pool, paid, kept),
                Err(reason) => self.marks[ix as usize] = Mark::Unmerged(reason),
            }
        }
        scratch.runs = runs;
        self.trace.sim_end_ns = utcnow_ns();
    }

    /// Whether `order`'s undroppable txs can still go in: none already in the block, and their
    /// gas limits and blobs within what is left.
    fn fits(&self, order: &Order, pool: &Pool) -> Result<(), UnmergedReason> {
        let (mut gas, mut blobs) = (0, 0);
        for (i, &tx) in pool.refs(order.txs).iter().enumerate() {
            if order.drop & (1 << i) != 0 {
                continue;
            }
            if self.in_block[tx as usize / 64] & (1 << (tx % 64)) != 0 {
                return Err(UnmergedReason::Duplicate);
            }
            let tx = pool.tx(tx);
            gas += tx.tx.gas_limit();
            blobs += tx.blobs().len() as u64;
        }
        if gas > self.gas_cap - self.cumulative || blobs > self.max_blobs - self.blobs {
            return Err(UnmergedReason::OutOfSpace);
        }
        Ok(())
    }

    /// Runs `order` on the current state under its inclusion rules: a tx that does not fit or
    /// cannot run is left out when the order allows it, a revert is kept only when allowed. Kept
    /// only when `keep`; otherwise, or on failure, the state is restored. Returns what the order
    /// paid the coinbase and which of its txs stayed.
    fn run(
        &mut self,
        order: &Order,
        pool: &Pool,
        cx: &Cx,
        keep: bool,
        keys: &mut Keys,
        scratch: &mut Scratch,
    ) -> Result<(U256, u32), UnmergedReason> {
        let txs = pool.refs(order.txs);
        if cx.blacklist.is_some_and(|list| txs.iter().any(|&tx| calls_listed(list, pool.tx(tx)))) {
            return Err(UnmergedReason::Invalid);
        }
        let coinbase = self.coinbase;
        let paid_before = self.state.account(keys, coinbase).balance;
        let gas_left = self.gas_cap - self.cumulative;
        let blobs_left = self.max_blobs - self.blobs;
        let (cumulative, receipts, start) = (self.cumulative, self.receipts.len(), self.state.at());
        let (mut gas, mut blobs, mut kept) = (0, 0, 0u32);
        let mut failed = None;
        for (i, &ix) in txs.iter().enumerate() {
            let tx = pool.tx(ix);
            let bit = 1u32 << i;
            let (can_drop, can_revert) = (order.drop & bit != 0, order.revert & bit != 0);
            let tx_blobs = tx.blobs().len() as u64;
            if tx.tx.gas_limit() > gas_left - gas || tx_blobs > blobs_left - blobs {
                if can_drop {
                    continue;
                }
                failed = Some(UnmergedReason::OutOfSpace);
                break;
            }
            let (at, mark) = (self.cumulative, self.state.at());
            let Some((tx_gas, succeeded)) = self.step(ix, pool, keys, scratch) else {
                // Invalid on this state (nonce, balance, ...): nothing was applied.
                self.cumulative = at;
                if can_drop || can_revert {
                    continue;
                }
                failed = Some(UnmergedReason::Invalid);
                break;
            };
            let listed = cx
                .blacklist
                .is_some_and(|list| touches_listed(list, keys, &self.state, mark, coinbase));
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
                self.truncate(self.receipts.len() - 1);
                self.cumulative = at;
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
            self.truncate(receipts);
            self.cumulative = cumulative;
        } else {
            self.state.forget();
        }
        if let Some(reason) = failed {
            return Err(reason);
        }
        if keep {
            self.blobs += blobs;
        }
        Ok((paid, kept))
    }

    /// Drops the receipts of the block's txs past its first `len`.
    fn truncate(&mut self, len: usize) {
        let own = self.receipts[len..].iter().filter(|r| r.0 == NONE).count();
        self.own.truncate(self.own.len() - own);
        self.receipts.truncate(len);
    }

    /// Puts pool tx `ix` next on the state: replays its effect when every value it read is the
    /// same, else runs it and records its effect. Pushes its receipt and returns its gas and
    /// whether it succeeded. `None` when it cannot go in here; nothing is then applied.
    fn step(
        &mut self,
        ix: u32,
        pool: &Pool,
        keys: &mut Keys,
        scratch: &mut Scratch,
    ) -> Option<(u64, bool)> {
        let (at, cumulative) = (self.state.at(), self.cumulative);
        let (vm, reads) = (&mut scratch.vm, &mut scratch.reads);
        if let Some(effect) = self.effects.holds(ix, keys, &self.state, self.coinbase) {
            let Some(gas) = self.effects.replay(effect, keys, &mut self.state, self.coinbase)
            else {
                self.state.undo_to(at);
                return None;
            };
            #[cfg(debug_assertions)]
            self.check_replay(ix, pool, keys, vm, at, gas);
            self.cumulative += gas;
            self.receipts.push((effect, self.cumulative));
            return Some((gas, self.effects.receipt(effect).0.succeeded));
        }
        let tx = pool.tx(ix);
        let before = self.state.account(keys, self.coinbase).balance;
        reads.accounts.clear();
        reads.slots.clear();
        reads.balances.clear();
        reads.floors.clear();
        reads.coinbase_address = false;
        let (header, gas_used) = (&self.base.header, &mut self.cumulative);
        let ran = vm.run(keys, &self.state, |evm| {
            evm.db.tx_reads = Some(std::mem::take(reads));
            let ran = evm.execute_tx(&tx.tx, header, gas_used, tx.sender);
            *reads = evm.db.tx_reads.take().unwrap_or_default();
            ran
        });
        let (receipt, report) = ran.ok()?;
        if let Err(reason) = self.state.fold(keys, &vm.evm.db) {
            metrics::rejection("merge_state", reason);
            self.state.undo_to(at);
            self.cumulative = cumulative;
            return None;
        }
        let (gas, succeeded) = (report.gas_spent, receipt.succeeded);
        let (state, own) = (&self.state, &mut self.own);
        let effect = self.effects.record(
            ix,
            gas,
            keys,
            state,
            at,
            reads,
            self.coinbase,
            before,
            receipt,
            own,
        );
        self.receipts.push((effect, self.cumulative));
        Some((gas, succeeded))
    }

    /// Runs pool tx `ix` where its effect was just replayed, from `at`, and checks the run writes
    /// the same values and gas. Leaves the run's writes, which are then the same.
    #[cfg(debug_assertions)]
    fn check_replay(
        &mut self,
        ix: u32,
        pool: &Pool,
        keys: &mut Keys,
        vm: &mut Vm,
        at: At,
        gas: u64,
    ) {
        let state = &self.state;
        let mut replayed: Vec<_> =
            state.accounts_since(at).map(|id| (id, state.account(keys, id))).collect();
        let mut replayed_slots: Vec<_> =
            state.slots_since(at).map(|id| (id, state.slot(keys, id))).collect();
        self.state.undo_to(at);
        // A replay may write a value unchanged; a fold writes only changes.
        replayed.retain(|&(id, v)| self.state.account(keys, id) != v);
        replayed_slots.retain(|&(id, v)| self.state.slot(keys, id) != v);
        let tx = pool.tx(ix);
        let mut cumulative = 0;
        let header = &self.base.header;
        let ran = vm.run(keys, &self.state, |evm| {
            evm.execute_tx(&tx.tx, header, &mut cumulative, tx.sender)
        });
        let (_, report) = ran.expect("replayed tx fails when run");
        self.state.fold(keys, &vm.evm.db).expect("replayed tx does not fold");
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

    /// Records `order` as committed by `run`: its kept txs join the block body.
    fn commit(&mut self, ix: u32, order: &Order, pool: &Pool, paid: U256, kept: u32) {
        for (i, &tx) in pool.refs(order.txs).iter().enumerate() {
            if kept & (1 << i) == 0 {
                continue;
            }
            self.appended.push(tx);
            self.in_block[tx as usize / 64] |= 1 << (tx % 64);
        }
        self.marks[ix as usize] = Mark::Taken;
        self.included.push((ix, kept));
        let origin = pool.sources[order.source as usize].address;
        let same = |r: &&mut Revenue| pool.sources[r.source as usize].address == origin;
        match self.revenues.iter_mut().find(same) {
            Some(revenue) => revenue.paid += paid,
            None => self.revenues.push(Revenue { source: order.source, paid }),
        }
    }

    /// The merged block with the distribution paying out the orders' revenue, when it beats the
    /// last emission by the configured margin. `Err` ends this base.
    pub fn emit(
        &mut self,
        pool: &Pool,
        cx: &Cx,
        scratch: &mut Scratch,
    ) -> Result<Option<MergedBlockV1>, &'static str> {
        let total: U256 = self.revenues.iter().map(|r| r.paid).sum();
        let base_fee = self.base.header.base_fee_per_gas.unwrap_or_default();
        let cost = U256::from(base_fee) * U256::from(cx.relay.distribution_gas_limit);
        if total <= cost {
            return Ok(None);
        }
        let coinbase = aaddr(self.base.header.coinbase);
        let to =
            payment::split(cx.relay, &self.revenues, &pool.sources, cost, cx.proposer, coinbase);
        let share = |address: Address| {
            to.iter().find(|(a, _)| *a == address).map(|(_, v)| *v).unwrap_or_default()
        };
        let proposer_value = pool.values[self.base.block as usize] + share(cx.proposer);
        let paid_out: U256 = to.iter().map(|(_, v)| *v).sum();
        let builder_revenue = total.saturating_sub(paid_out).saturating_sub(cost);
        if builder_revenue.is_zero() || proposer_value <= self.best + cx.min_increase {
            return Ok(None);
        }

        let keys = &mut *cx.parent.keys.borrow_mut();
        let safe = eaddr(self.safe);
        let signer = eaddr(cx.signer.address());
        let (safe_id, signer_id) = (keys.account(safe)?, keys.account(signer)?);
        let nonce_id = keys.slot(safe, H256::from_low_u64_be(SAFE_NONCE_SLOT))?;
        let p = SafePayment {
            safe: self.safe,
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

        // The distribution, requests and withdrawals run in one database over the state, and are
        // folded in only for the state root: the live state keeps taking orders.
        let header = &self.base.header;
        let prague = cx.parent.chain.is_prague_activated(header.timestamp);
        // Requests read receipts only for deposit contract logs.
        let deposits = &mut scratch.deposits;
        deposits.clear();
        let deposit = cx.parent.chain.deposit_contract_address;
        for (receipt, ..) in block_receipts(&self.receipts, &self.own, &self.effects) {
            if prague && receipt.logs.iter().any(|log| log.address == deposit) {
                deposits.push(receipt.clone());
            }
        }
        let withdrawals = &self.base.withdrawals;
        let mut cumulative = self.cumulative;
        let ran = scratch.vm.run(keys, &self.state, |evm| {
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
        let at = self.state.at();
        let folded = self.state.fold(keys, &scratch.vm.evm.db);
        let state_root = folded.and_then(|()| {
            let nodes = &mut *cx.nodes.borrow_mut();
            let (store, db) = (cx.store, cx.parent.db.as_ref());
            let open = |nodes: &mut Arena| Root::open(store, cx.parent.header.state_root, nodes);
            let reused = self.root.is_some();
            let mut root = match self.root.take() {
                Some(root) => root,
                None => open(nodes)?,
            };
            let mut state_root = root.update(store, db, nodes, keys, &self.state);
            // Writing back the last block's keys can need parent nodes that writing from the
            // parent does not, e.g. a sibling when deleting a slot it set.
            if state_root.is_err() && reused {
                metrics::rejection("state_root", "reopened");
                root = open(nodes)?;
                state_root = root.update(store, db, nodes, keys, &self.state);
            }
            self.root = state_root.is_ok().then_some(root);
            state_root
        });
        self.state.undo_to(at);
        let state_root = state_root?;

        let base = pool.refs(self.base.txs).iter();
        let mut txs = Vec::with_capacity(self.receipts.len() + 1);
        txs.extend(base.chain(&self.appended).map(|&tx| pool.tx(tx).encoded.clone()));
        txs.push(encoded.into());
        let paid = bloom_from_logs(&receipt.logs, &NativeCrypto);
        let paid = (&receipt, cumulative, &paid, None);
        let mut logs_bloom = Bloom::zero();
        self.receipt_bytes.clear();
        self.receipt_at.clear();
        self.receipt_at.push(0);
        let receipts = block_receipts(&self.receipts, &self.own, &self.effects);
        for (receipt, gas, bloom, logs) in receipts.chain([paid]) {
            logs_bloom |= *bloom;
            match logs {
                Some(logs) => put_receipt(&mut self.receipt_bytes, receipt, gas, bloom, logs),
                None => {
                    let encoded = receipt.encode_inner_with_precomputed_bloom(*bloom);
                    self.receipt_bytes.extend_from_slice(&encoded);
                }
            }
            self.receipt_at.push(self.receipt_bytes.len());
        }
        let (bytes, at) = (&self.receipt_bytes, &self.receipt_at);
        let mut header = self.base.header.clone();
        header.state_root = state_root;
        header.transactions_root = self.lists.root(txs.len(), |i| &txs[i]);
        header.receipts_root = self.lists.root(at.len() - 1, |i| &bytes[at[i]..at[i + 1]]);
        header.logs_bloom = logs_bloom;
        header.gas_used = cumulative;
        header.blob_gas_used = Some(self.blobs * u64::from(GAS_PER_BLOB));
        if prague {
            header.requests_hash = Some(compute_requests_hash(&requests));
        }
        let execution_payload = payload_v3(&header, txs, &self.base.withdrawals);

        let inclusions = self
            .revenues
            .iter()
            .map(|r| {
                let source = &pool.sources[r.source as usize];
                BuilderInclusion {
                    builder_pubkey: source.pubkey,
                    origin_coinbase: source.address,
                    contribution: r.paid,
                    revenue: share(source.address),
                    txs: self
                        .included
                        .iter()
                        .filter(|&&(ix, _)| {
                            let order = &pool.orders[ix as usize];
                            pool.sources[order.source as usize].address == source.address
                        })
                        .flat_map(|&(ix, kept)| {
                            let txs = pool.refs(pool.orders[ix as usize].txs).iter().enumerate();
                            txs.filter(move |&(i, _)| kept & (1 << i) != 0)
                                .map(|(_, &tx)| pool.tx(tx).hash)
                        })
                        .collect(),
                }
            })
            .collect();
        let unmerged = self.marks.iter().zip(&pool.orders).flat_map(|(&mark, order)| {
            let reason = match mark {
                Mark::Unmerged(reason) => Some(reason),
                Mark::Open | Mark::Taken => None,
            };
            reason.into_iter().flat_map(|reason| {
                pool.refs(order.txs)
                    .iter()
                    .map(move |&tx| UnmergedTx { tx_hash: pool.tx(tx).hash, reason })
            })
        });
        self.trace.finalize_ns = utcnow_ns();
        self.best = proposer_value;
        self.emissions += 1;
        Ok(Some(MergedBlockV1 {
            slot: cx.slot,
            response_id: 0,
            base_block_hash: self.base.hash,
            base_builder_pubkey: pool.sources[self.base.source as usize].pubkey,
            execution_payload,
            execution_requests: requests_to_v4(&requests).map_err(|_| "requests")?,
            appended_blobs: self
                .appended
                .iter()
                .flat_map(|&tx| pool.tx(tx).blobs().iter().map(|h| b256(*h)))
                .collect(),
            proposer_value,
            base_builder_revenue: builder_revenue,
            relay_revenue: share(cx.relay.relay_fee_recipient),
            builder_inclusions: inclusions,
            included_order_ids: self
                .included
                .iter()
                .map(|&(ix, _)| pool.ids[ix as usize])
                .collect(),
            trace: self.trace,
            unmerged_txs: unmerged.collect(),
        }))
    }
}

/// The block's receipts in order, each with its cumulative gas, logs bloom and, when an effect
/// holds it, its logs encoded.
fn block_receipts<'a>(
    receipts: &'a [(u32, u64)],
    own: &'a [(Receipt, Bloom)],
    effects: &'a Effects,
) -> impl Iterator<Item = (&'a Receipt, u64, &'a Bloom, Option<&'a [u8]>)> {
    let mut own = own.iter();
    receipts.iter().map(move |&(effect, gas)| match effect {
        NONE => {
            let (receipt, bloom) = own.next().expect("an own receipt per NONE");
            (receipt, gas, bloom, None)
        }
        _ => {
            let (receipt, bloom, logs) = effects.receipt(effect);
            (receipt, gas, bloom, Some(logs))
        }
    })
}

fn calls_listed(list: &DashSet<Address>, tx: &Tx) -> bool {
    matches!(tx.tx.to(), TxKind::Call(to) if list.contains(&aaddr(to)))
}

/// Whether what `state` holds written since `at` changed a listed account other than the coinbase
/// (account id `coinbase`).
fn touches_listed(
    list: &DashSet<Address>,
    keys: &Keys,
    state: &State,
    at: At,
    coinbase: u32,
) -> bool {
    if list.is_empty() {
        return false;
    }
    let listed = |a: ethrex_common::Address| list.contains(&aaddr(a));
    state.accounts_since(at).any(|id| id != coinbase && listed(keys.account_at(id).0)) ||
        state.slots_since(at).any(|id| listed(keys.slot_at(id).0))
}
