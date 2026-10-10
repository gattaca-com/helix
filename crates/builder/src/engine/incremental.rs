//! B: builds a base as a delta against the same builder's previous base instead of replaying it
//! from the parent. Each tx is kept as its effect (what it read, what it wrote); a new base is
//! aligned to the previous one and only txs whose reads changed are run again.

use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
    time::Instant,
};

use alloy_primitives::B256;
use ethrex_blockchain::payload::PayloadBuildContext;
use ethrex_common::{
    Address, H256, U256,
    constants::{EMPTY_TRIE_HASH, GAS_PER_BLOB},
    types::{
        AccountState, BlockHeader, ChainConfig, Code, CodeMetadata, Log, Receipt, Transaction,
    },
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::{
    db::{
        Database,
        gen_db::{AccountSnapshot, GeneralizedDatabase, TxReads},
    },
    errors::{DatabaseError, InternalError},
    vm::VMType,
};
use ethrex_vm::Evm;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Key {
    Account(Address),
    Slot(Address, H256),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Val {
    Account { balance: U256, nonce: u64, code_hash: H256, exists: bool, has_storage: bool },
    Slot(U256),
}

impl Val {
    pub fn of_snapshot(snapshot: &AccountSnapshot) -> Self {
        Self::of(snapshot)
    }

    fn of(snapshot: &AccountSnapshot) -> Self {
        Val::Account {
            balance: snapshot.info.balance,
            nonce: snapshot.info.nonce,
            code_hash: snapshot.info.code_hash,
            exists: snapshot.exists,
            has_storage: snapshot.has_storage,
        }
    }
}

/// What one tx read (first value seen) and wrote (final value), coinbase excluded.
#[derive(Clone)]
pub struct Effect {
    pub hash: B256,
    pub reads: Vec<(Key, Val)>,
    pub writes: Vec<(Key, Val)>,
    pub reads_coinbase: bool,
    pub credit: U256,
    /// What the tx took from the coinbase (the builder's own payments).
    pub debit: U256,
    /// The coinbase's nonce after the tx, when the tx changed it (the builder signs txs too).
    pub coinbase_nonce: Option<u64>,
    /// The coinbase's nonce and balance as the tx found them, and whether the tx itself
    /// inspected that balance. One that did not only needed the balance to cover its cost.
    pub coinbase_seen: Option<(u64, U256, bool)>,
    /// The coinbase the tx ran under, and whether it read that address (`COINBASE`).
    pub coinbase: Address,
    pub coinbase_address: bool,
    /// Code deployed by the tx, for later txs to call.
    pub codes: Vec<Code>,
    pub outcome: Outcome,
}

/// A tx's receipt and the gas it moved: block gas (`gas_used`) and receipt gas (`gas_spent`).
#[derive(Clone, PartialEq, Debug)]
pub struct Outcome {
    pub succeeded: bool,
    pub gas_used: u64,
    pub gas_spent: u64,
    pub logs: Vec<Log>,
}

impl Effect {
    pub fn sort(&mut self) {
        self.reads.sort_by_key(|(k, _)| *k);
        self.writes.sort_by_key(|(k, _)| *k);
    }

    pub fn from_db(
        hash: B256,
        db: &GeneralizedDatabase,
        reads: &TxReads,
        coinbase: Address,
        (credit, debit): (U256, U256),
        outcome: Outcome,
    ) -> Option<Self> {
        let mut out = Effect {
            hash,
            reads: Vec::with_capacity(reads.accounts.len() + reads.slots.len()),
            writes: Vec::new(),
            reads_coinbase: reads.accounts.contains_key(&coinbase),
            credit,
            debit,
            coinbase_nonce: None,
            coinbase_seen: None,
            coinbase,
            coinbase_address: reads.coinbase_address,
            codes: Vec::new(),
            outcome,
        };
        if let Some(before) = reads.accounts.get(&coinbase) {
            let after = db.current_accounts_state.get(&coinbase)?.info.nonce;
            out.coinbase_nonce = (after != before.info.nonce).then_some(after);
            out.coinbase_seen =
                Some((before.info.nonce, before.info.balance, reads.balances.contains(&coinbase)));
        }
        for (address, before) in &reads.accounts {
            if *address == coinbase {
                continue;
            }
            let key = Key::Account(*address);
            let before = Val::of(before);
            let after = AccountSnapshot::of(db.current_accounts_state.get(address)?);
            if after.info.code_hash != before_code(&before) {
                out.codes.push(db.codes.get(&after.info.code_hash)?.clone());
            }
            let after = Val::of(&after);
            if after != before {
                out.writes.push((key, after));
            }
            out.reads.push((key, before));
        }
        for (&(address, slot), before) in &reads.slots {
            let key = Key::Slot(address, slot);
            let after = *db.current_accounts_state.get(&address)?.storage.get(&slot)?;
            if after != *before {
                out.writes.push((key, Val::Slot(after)));
            }
            out.reads.push((key, Val::Slot(*before)));
        }
        out.sort();
        Some(out)
    }
}

fn before_code(val: &Val) -> H256 {
    match val {
        Val::Account { code_hash, .. } => *code_hash,
        Val::Slot(_) => H256::zero(),
    }
}

thread_local! {
    /// Effects of the base being replayed on this thread, in order; `None` when not recording.
    static SINK: RefCell<Option<Vec<Option<Arc<Effect>>>>> = const { RefCell::new(None) };
}

thread_local! {
    /// Txs A executed (rather than reused) in the base being recorded.
    pub static A_EXECUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn start_recording() {
    A_EXECUTED.with(|c| c.set(0));
    A_REUSED.with(|r| r.borrow_mut().clear());
    SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
}

pub fn shadow_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("B_SHADOW").is_some());
    *ON
}

pub fn recording() -> bool {
    SINK.with(|sink| sink.borrow().is_some())
}

thread_local! {
    /// Per tx of the base being recorded, whether A reused it rather than running it.
    pub static A_REUSED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
}

pub fn push(effect: Option<Arc<Effect>>) {
    SINK.with(|sink| {
        if let Some(sink) = sink.borrow_mut().as_mut() {
            sink.push(effect);
        }
    });
}

pub fn take() -> Option<Vec<Option<Arc<Effect>>>> {
    SINK.with(|sink| sink.borrow_mut().take())
}

/// A base kept for later ones to diff against.
pub struct Prior {
    effects: Vec<Arc<Effect>>,
    position: FxHashMap<B256, usize>,
    /// Every write per key in tx order.
    history: FxHashMap<Key, Vec<(usize, Val)>>,
    /// Coinbase credit before each tx.
    credits: Vec<U256>,
}

impl Prior {
    fn new(effects: Vec<Arc<Effect>>, parents: &mut FxHashMap<Key, Val>) -> Self {
        let mut history: FxHashMap<Key, Vec<(usize, Val)>> = FxHashMap::default();
        let mut credits = Vec::with_capacity(effects.len() + 1);
        let mut credit = U256::zero();
        for (ix, effect) in effects.iter().enumerate() {
            credits.push(credit);
            credit = credit.saturating_add(effect.credit);
            for (key, val) in &effect.reads {
                if !history.contains_key(key) {
                    parents.entry(*key).or_insert_with(|| val.clone());
                }
            }
            for (key, val) in &effect.writes {
                history.entry(*key).or_default().push((ix, val.clone()));
            }
        }
        credits.push(credit);
        let position = effects.iter().enumerate().map(|(ix, e)| (e.hash, ix)).collect();
        Prior { effects, position, history, credits }
    }

    /// The key's value in this run just before tx `at`; `None` when unwritten so far.
    fn before(&self, key: &Key, at: usize) -> Option<&Val> {
        let writes = self.history.get(key)?;
        let n = writes.partition_point(|(ix, _)| *ix < at);
        n.checked_sub(1).map(|n| &writes[n].1)
    }

    /// The payload context after the new run: `template` with every key the old or new run wrote
    /// set to its final value, the coinbase's totals, and the applied effects' receipts.
    fn context(
        &self,
        mut ctx: PayloadBuildContext,
        report: &Report,
        exec: &Exec,
    ) -> Result<PayloadBuildContext, InternalError> {
        let finals = self
            .history
            .keys()
            .filter(|key| !report.dirty.contains_key(key))
            .map(|key| (key, self.before(key, self.effects.len()).cloned()))
            .chain(report.dirty.iter().map(|(key, val)| (key, val.clone())));
        let db = &mut ctx.vm.db;
        for (key, val) in finals {
            match (key, val) {
                (_, None) => {}
                (
                    Key::Account(address),
                    Some(Val::Account { balance, nonce, code_hash, exists, has_storage }),
                ) => {
                    db.get_account(*address)?;
                    let account = db.get_account_mut(*address)?;
                    account.info.balance = balance;
                    account.info.nonce = nonce;
                    account.info.code_hash = code_hash;
                    account.exists = exists;
                    account.has_storage = has_storage;
                }
                (Key::Slot(address, slot), Some(Val::Slot(value))) => {
                    db.get_account(*address)?;
                    db.storage_value(*address, *slot)?;
                    db.get_account_mut(*address)?.storage.insert(*slot, value);
                }
                _ => return Err(InternalError::Custom("key and value kinds differ".into())),
            }
        }
        db.get_account(exec.coinbase)?;
        let coinbase = db.get_account_mut(exec.coinbase)?;
        coinbase.info.balance =
            coinbase.info.balance.saturating_add(report.credit).saturating_sub(report.debit);
        if let Some(nonce) = report.coinbase_nonce {
            coinbase.info.nonce = nonce;
        }
        let base_fee = ctx.payload.header.base_fee_per_gas;
        for ((tx, _), effect) in exec.txs.iter().zip(&report.applied) {
            for code in &effect.codes {
                ctx.vm.db.codes.entry(code.hash).or_insert_with(|| code.clone());
            }
            let outcome = &effect.outcome;
            ctx.remaining_gas = ctx.remaining_gas.saturating_sub(outcome.gas_used);
            ctx.cumulative_gas_spent += outcome.gas_spent;
            ctx.block_value +=
                U256::from(outcome.gas_spent) * tx.effective_gas_tip(base_fee).unwrap_or_default();
            let blobs = tx.blob_versioned_hashes().len() as u64;
            if blobs > 0 {
                ctx.payload.header.blob_gas_used = Some(
                    ctx.payload.header.blob_gas_used.unwrap_or_default() +
                        blobs * u64::from(GAS_PER_BLOB),
                );
            }
            ctx.receipts.push(Receipt::new(
                tx.tx_type(),
                outcome.succeeded,
                ctx.cumulative_gas_spent,
                outcome.logs.clone(),
            ));
            ctx.payload.body.transactions.push(tx.clone());
        }
        Ok(ctx)
    }

    /// Common txs kept in relative order: per new tx, its position here when on that path.
    fn align(&self, new: &[Arc<Effect>]) -> Vec<Option<usize>> {
        let olds: Vec<Option<usize>> =
            new.iter().map(|e| self.position.get(&e.hash).copied()).collect();
        let mut tails: Vec<usize> = Vec::new();
        let mut tail_ix: Vec<usize> = Vec::new();
        let mut parent: Vec<Option<usize>> = vec![None; new.len()];
        for (i, p) in olds.iter().enumerate() {
            let Some(p) = *p else { continue };
            let at = tails.partition_point(|t| *t < p);
            parent[i] = at.checked_sub(1).map(|a| tail_ix[a]);
            if at == tails.len() {
                tails.push(p);
                tail_ix.push(i);
            } else {
                tails[at] = p;
                tail_ix[at] = i;
            }
        }
        let mut aligned = vec![None; new.len()];
        let mut cursor = tail_ix.last().copied();
        while let Some(i) = cursor {
            aligned[i] = olds[i];
            cursor = parent[i];
        }
        aligned
    }
}

/// How the new base differed and what B had to do for it.
#[derive(Default)]
pub struct Report {
    pub txs: usize,
    pub run: usize,
    pub skipped: usize,
    pub delta: usize,
    pub micros: u64,
    pub wrong: usize,
    /// Run txs by kind: aligned, moved, not in the prior.
    pub causes: [usize; 3],
    /// Txs B ran itself whose effect differed from A's, or that it could not run.
    pub exec_wrong: usize,
    pub exec_failed: usize,
    pub exec_micros: u64,
    pub dirty: FxHashMap<Key, Option<Val>>,
    pub credit: U256,
    pub debit: U256,
    pub coinbase_nonce: Option<u64>,
    pub root_micros: u64,
    pub root: Option<H256>,
    /// A builder's first base: no prior, so the trie is built from the parent.
    pub first: bool,
    /// The trie was rebuilt from the parent: a first base, or a patch that failed.
    pub rebuilt: bool,
    pub run_leads: Vec<u64>,
    pub run_novel: usize,
    /// Every tx but the payment appeared before the base: how long before the last of them did.
    pub content_lead: Option<u64>,
    /// Whether the builder's previous base came from the same pubkey.
    pub same_stream: Option<bool>,
    /// Txs B ran that A reused: no matching effect known to B, coinbase check failed, a read B
    /// could not resolve, a read B resolved to another value, or none of these.
    pub missed: [usize; 6],
    pub best_any: Option<usize>,
    pub stream_first: bool,
    /// When the base arrived, relative to the start of its slot.
    pub arrival_ms: Option<i64>,
    /// The longest chain of run txs each reading what an earlier run tx wrote.
    pub chain: usize,
    /// The effect B applied per tx, in base order.
    pub applied: Vec<Arc<Effect>>,
    /// Txs whose applied receipt differs from A's.
    pub outcome_wrong: usize,
    pub ctx_micros: u64,
    /// B's payload context after the base, built over the builder's previous base.
    pub ctx: Option<PayloadBuildContext>,
}

/// Slot-wide knowledge B may use: parent values seen in reads, and every effect seen per tx.
#[derive(Default)]
pub struct Known {
    parents: FxHashMap<Key, Val>,
    effects: FxHashMap<B256, Vec<Arc<Effect>>>,
    codes: Arc<FxHashMap<H256, Code>>,
    /// Storage the slot's system calls wrote before any tx, which the parent store predates.
    system: Arc<FxHashMap<Key, Val>>,
    /// Every (tx, reads) B ever stored, evicted or not.
    ever: rustc_hash::FxHashSet<u64>,
}

fn variant_id(e: &Effect) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    e.hash.hash(&mut h);
    format!("{:?}", e.reads).hash(&mut h);
    h.finish()
}

/// What B needs to run a tx itself: the base's txs, its header and the parent state.
pub struct Exec {
    pub txs: Vec<(Transaction, Address)>,
    pub header: BlockHeader,
    pub store: Arc<dyn Database>,
    pub coinbase: Address,
    pub trie_store: ethrex_storage::Store,
    pub parent_root: H256,
    pub base_hash: B256,
    pub pubkey: [u8; 48],
    pub slot: u64,
    /// The payload context with the slot's system calls applied and no txs yet.
    pub template: Option<PayloadBuildContext>,
}

const MAINNET_GENESIS: i64 = 1_606_824_023;

/// When each tx first appeared in any block, and when each block arrived (recording clock).
static TIMELINE: Mutex<Option<(FxHashMap<B256, u64>, FxHashMap<B256, u64>)>> = Mutex::new(None);

pub fn set_timeline(first_seen: FxHashMap<B256, u64>, arrived: FxHashMap<B256, u64>) {
    if let Ok(mut t) = TIMELINE.lock() {
        *t = Some((first_seen, arrived));
    }
}

/// How long before `base` arrived `tx` first appeared, if it did.
fn lead(tx: &B256, base: &B256) -> Option<Option<u64>> {
    let t = TIMELINE.lock().ok()?;
    let (first_seen, arrived) = t.as_ref()?;
    let at = *arrived.get(base)?;
    Some(first_seen.get(tx).filter(|seen| **seen < at).map(|seen| (at - seen) / 1_000_000))
}

/// The new run's state just before one tx: diverged keys, else the prior run at that point,
/// else the parent.
struct View {
    prior: Arc<Prior>,
    at: usize,
    dirty: FxHashMap<Key, Option<Val>>,
    coinbase: Address,
    coinbase_balance: U256,
    coinbase_nonce: Option<u64>,
    codes: Arc<FxHashMap<H256, Code>>,
    system: Arc<FxHashMap<Key, Val>>,
    store: Arc<dyn Database>,
}

impl View {
    fn val(&self, key: &Key) -> Option<Val> {
        match self.dirty.get(key) {
            Some(v) => v.clone(),
            None => self.prior.before(key, self.at).cloned(),
        }
    }
}

impl Database for View {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        if address == self.coinbase {
            let mut state = self.store.get_account_state(address)?;
            state.balance = self.coinbase_balance;
            if let Some(nonce) = self.coinbase_nonce {
                state.nonce = nonce;
            }
            return Ok(state);
        }
        match self.val(&Key::Account(address)) {
            Some(Val::Account { balance, nonce, code_hash, exists, has_storage }) => {
                if !exists {
                    return Ok(AccountState::default());
                }
                Ok(AccountState {
                    nonce,
                    balance,
                    storage_root: if has_storage { H256::zero() } else { *EMPTY_TRIE_HASH },
                    code_hash,
                })
            }
            _ => self.store.get_account_state(address),
        }
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        let slot = Key::Slot(address, key);
        match self.val(&slot).or_else(|| self.system.get(&slot).cloned()) {
            Some(Val::Slot(v)) => Ok(v),
            _ => self.store.get_storage_value(address, key),
        }
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        self.store.get_block_hash(block_number)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.store.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        match self.codes.get(&code_hash) {
            Some(code) => Ok(code.clone()),
            None => self.store.get_account_code(code_hash),
        }
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        match self.codes.get(&code_hash) {
            Some(code) => Ok(CodeMetadata { length: code.len() as u64 }),
            None => self.store.get_code_metadata(code_hash),
        }
    }

    fn precompile_cache(&self) -> Option<&ethrex_levm::precompiles::PrecompileCache> {
        self.store.precompile_cache()
    }
}

/// Runs new tx `i` on B's view of the new run just before it.
fn execute(view: View, exec: &Exec, i: usize, hash: B256) -> Option<Effect> {
    let (tx, sender) = exec.txs.get(i)?;
    let coinbase = view.coinbase;
    let coinbase_before = view.coinbase_balance;
    let mut vm = Evm {
        db: GeneralizedDatabase::new(Arc::new(view)),
        vm_type: VMType::L1,
        crypto: Arc::new(NativeCrypto),
        stateless_validator: None,
    };
    vm.db.tx_reads = Some(Default::default());
    let mut gas = 0;
    let (receipt, ran) = vm.execute_tx(tx, &exec.header, &mut gas, *sender).ok()?;
    vm.db.reads_paused = true;
    let coinbase_after = vm.db.get_account(coinbase).ok()?.info.balance;
    let reads = vm.db.tx_reads.take()?;
    Effect::from_db(
        hash,
        &vm.db,
        &reads,
        coinbase,
        (
            coinbase_after.saturating_sub(coinbase_before),
            coinbase_before.saturating_sub(coinbase_after),
        ),
        Outcome {
            succeeded: receipt.succeeded,
            gas_used: ran.gas_used,
            gas_spent: ran.gas_spent,
            logs: receipt.logs,
        },
    )
}

const VARIANTS: usize = 16;

/// Diffs `new` against `prior`. A tx is skipped when one of its known effects still holds on B's
/// state at that point; otherwise it is run (here: taken from `new`, which is A's ground truth).
/// B's resulting state is checked against A's.
pub fn diff(prior: &Arc<Prior>, known: &Known, new: &[Arc<Effect>], exec: &Exec) -> Report {
    let start = Instant::now();
    let old = &prior.effects;
    let aligned = prior.align(new);
    // A parent value no earlier tx read comes from the parent store, through the slot's system
    // calls, and is kept for the rest of this base.
    let fetched: RefCell<FxHashMap<Key, Option<Val>>> = RefCell::new(FxHashMap::default());
    let parent = |key: &Key| -> Option<Val> {
        if let Some(v) = known.parents.get(key).or_else(|| known.system.get(key)) {
            return Some(v.clone());
        }
        if let Some(v) = fetched.borrow().get(key) {
            return v.clone();
        }
        let v = match key {
            Key::Account(address) => {
                exec.store.get_account_state(*address).ok().map(|state| Val::Account {
                    balance: state.balance,
                    nonce: state.nonce,
                    code_hash: state.code_hash,
                    exists: state != AccountState::default(),
                    has_storage: state.storage_root != *EMPTY_TRIE_HASH,
                })
            }
            Key::Slot(address, slot) => {
                exec.store.get_storage_value(*address, *slot).ok().map(Val::Slot)
            }
        };
        fetched.borrow_mut().insert(*key, v.clone());
        v
    };
    let resolve =
        |key: &Key, v: Option<&Val>| -> Option<Val> { v.cloned().or_else(|| parent(key)) };
    // `dirty[k]`: k's value in the new run where it differs from the old run at `optr`.
    let mut dirty: FxHashMap<Key, Option<Val>> = FxHashMap::default();
    let mut optr = 0usize;
    let mut new_credit = U256::zero();
    let mut new_debit = U256::zero();
    let (coinbase_parent, coinbase_parent_nonce) = exec
        .store
        .get_account_state(exec.coinbase)
        .map(|s| (s.balance, s.nonce))
        .unwrap_or_default();
    let mut ran: Vec<Arc<Effect>> = Vec::new();
    let mut coinbase_nonce: Option<u64> = None;
    let mut report = Report { txs: new.len(), ..Default::default() };

    for (i, effect) in new.iter().enumerate() {
        // Old txs before the aligned one ran only in the old sequence.
        if let Some(p) = aligned[i] {
            for j in optr..p {
                for (key, _) in &old[j].writes {
                    if !dirty.contains_key(key) {
                        let v = resolve(key, prior.before(key, j));
                        dirty.insert(*key, v);
                    }
                }
            }
            optr = p;
        }
        let credit_same = new_credit == prior.credits[optr];
        let now = |dirty: &FxHashMap<Key, Option<Val>>, key: &Key| -> Option<Val> {
            match dirty.get(key) {
                Some(v) => v.clone(),
                None => resolve(key, prior.before(key, optr)),
            }
        };
        let _ = credit_same;
        // A tx that read the coinbase still holds if it found the same nonce, did not inspect the
        // balance, and the balance now covers at least what it covered then.
        let coinbase_balance = coinbase_parent.saturating_add(new_credit).saturating_sub(new_debit);
        let coinbase_now = coinbase_nonce.unwrap_or(coinbase_parent_nonce);
        // Under another coinbase an effect carries over only if it never involved this one: its
        // reads and writes leave the coinbase out, so they would not show the difference.
        let foreign = |e: &Effect| {
            let mine = Key::Account(exec.coinbase);
            e.coinbase != exec.coinbase &&
                (e.reads_coinbase ||
                    e.coinbase_address ||
                    e.reads.iter().any(|(k, _)| *k == mine) ||
                    e.writes.iter().any(|(k, _)| *k == mine))
        };
        let coinbase_ok = |e: &Effect| match e.coinbase_seen {
            _ if foreign(e) => false,
            _ if !e.reads_coinbase => true,
            Some((nonce, balance, inspected)) => {
                !inspected && nonce == coinbase_now && coinbase_balance >= balance
            }
            None => false,
        };
        let holds = |e: &Effect, dirty: &FxHashMap<Key, Option<Val>>| {
            coinbase_ok(e) && e.reads.iter().all(|(key, val)| now(dirty, key).as_ref() == Some(val))
        };
        // The aligned old effect holds unless a read is dirty, so check only those.
        let same_old = aligned[i].is_some_and(|p| {
            coinbase_ok(&old[p]) &&
                old[p]
                    .reads
                    .iter()
                    .all(|(key, val)| dirty.get(key).is_none_or(|now| now.as_ref() == Some(val)))
        });
        let chosen: Option<&Arc<Effect>> = if same_old {
            aligned[i].map(|p| &old[p])
        } else {
            prior
                .position
                .get(&effect.hash)
                .map(|p| &old[*p])
                .into_iter()
                .chain(known.effects.get(&effect.hash).into_iter().flatten())
                .find(|e| holds(e, &dirty))
        };
        let applied: Arc<Effect> = match chosen {
            Some(e) => {
                report.skipped += 1;
                if e.writes != effect.writes ||
                    e.credit != effect.credit ||
                    e.debit != effect.debit ||
                    e.coinbase_nonce != effect.coinbase_nonce
                {
                    report.wrong += 1;
                }
                e.clone()
            }
            None => {
                report.run += 1;
                if A_REUSED.with(|r| r.borrow().get(i).copied().unwrap_or(false)) {
                    let variants = known.effects.get(&effect.hash);
                    let same = variants.and_then(|v| v.iter().find(|v| v.reads == effect.reads));
                    let in_prior = prior
                        .position
                        .get(&effect.hash)
                        .map(|p| &old[*p])
                        .filter(|v| v.reads == effect.reads);
                    match same.or(in_prior) {
                        None if known.ever.contains(&variant_id(effect)) => report.missed[5] += 1,
                        None => report.missed[0] += 1,
                        Some(v) if !coinbase_ok(v) => report.missed[1] += 1,
                        Some(v) => {
                            let bad = v
                                .reads
                                .iter()
                                .find(|(k, val)| now(&dirty, k).as_ref() != Some(val));
                            match bad.map(|(k, _)| now(&dirty, k)) {
                                Some(None) => report.missed[2] += 1,
                                Some(Some(_)) => report.missed[3] += 1,
                                None => report.missed[4] += 1,
                            }
                        }
                    }
                }
                match lead(&effect.hash, &exec.base_hash) {
                    Some(Some(ms)) => report.run_leads.push(ms),
                    Some(None) => report.run_novel += 1,
                    None => {}
                }
                let kind = match (aligned[i], prior.position.contains_key(&effect.hash)) {
                    (Some(_), _) => 0,
                    (None, true) => 1,
                    (None, false) => 2,
                };
                report.causes[kind] += 1;
                let exec_start = Instant::now();
                let view = View {
                    prior: prior.clone(),
                    at: optr,
                    dirty: dirty.clone(),
                    coinbase: exec.coinbase,
                    coinbase_balance: coinbase_parent
                        .saturating_add(new_credit)
                        .saturating_sub(new_debit),
                    coinbase_nonce,
                    codes: known.codes.clone(),
                    system: known.system.clone(),
                    store: exec.store.clone(),
                };
                let own = execute(view, exec, i, effect.hash);
                report.exec_micros += exec_start.elapsed().as_micros() as u64;
                match own {
                    Some(own) => {
                        if own.writes != effect.writes ||
                            own.credit != effect.credit ||
                            own.debit != effect.debit ||
                            own.coinbase_nonce != effect.coinbase_nonce
                        {
                            report.exec_wrong += 1;
                        }
                        let own = Arc::new(own);
                        ran.push(own.clone());
                        own
                    }
                    None => {
                        report.exec_failed += 1;
                        effect.clone()
                    }
                }
            }
        };
        if applied.outcome != effect.outcome {
            report.outcome_wrong += 1;
        }
        new_credit = new_credit.saturating_add(applied.credit);
        new_debit = new_debit.saturating_add(applied.debit);
        if applied.coinbase_nonce.is_some() {
            coinbase_nonce = applied.coinbase_nonce;
        }
        if same_old {
            for (key, _) in &applied.writes {
                dirty.remove(key);
            }
            optr += 1;
            report.applied.push(applied);
            continue;
        }
        // Keys either run wrote may now differ between the two runs.
        let mut keys: Vec<Key> = applied.writes.iter().map(|(k, _)| *k).collect();
        let advance = aligned[i].is_some();
        if advance {
            keys.extend(old[optr].writes.iter().map(|(k, _)| *k));
        }
        let values: Vec<(Key, Option<Val>)> = keys
            .iter()
            .map(|key| {
                let v = match applied.writes.iter().find(|(k, _)| k == key) {
                    Some((_, v)) => Some(v.clone()),
                    None => now(&dirty, key),
                };
                (*key, v)
            })
            .collect();
        if advance {
            optr += 1;
        }
        for (key, v) in values {
            if resolve(&key, prior.before(&key, optr)) == v && v.is_some() {
                dirty.remove(&key);
            } else {
                dirty.insert(key, v);
            }
        }
        report.applied.push(applied);
    }
    for j in optr..old.len() {
        for (key, _) in &old[j].writes {
            if !dirty.contains_key(key) {
                let v = resolve(key, prior.before(key, j));
                dirty.insert(*key, v);
            }
        }
    }
    report.micros = start.elapsed().as_micros() as u64;
    report.credit = new_credit;
    report.debit = new_debit;
    report.coinbase_nonce = coinbase_nonce;

    // Ground truth: A's final state for the new base, against the old final state plus `dirty`.
    let mut a_final: FxHashMap<Key, Val> = FxHashMap::default();
    for effect in new {
        for (key, val) in &effect.writes {
            a_final.insert(*key, val.clone());
        }
    }
    let mut keys: Vec<Key> = a_final.keys().copied().collect();
    keys.extend(prior.history.keys().copied());
    keys.extend(dirty.keys().copied());
    keys.sort();
    keys.dedup();
    for key in &keys {
        let b = match dirty.get(key) {
            Some(v) => v.clone(),
            None => resolve(key, prior.before(key, old.len())),
        };
        let a = resolve(key, a_final.get(key));
        if b != a {
            report.wrong += 1;
        }
    }
    {
        let mut depth: Vec<(rustc_hash::FxHashSet<Key>, bool, usize)> = Vec::new();
        for e in &ran {
            let reads: rustc_hash::FxHashSet<Key> = e.reads.iter().map(|(k, _)| *k).collect();
            let d = 1 + depth
                .iter()
                .filter(|(writes, credit, _)| {
                    writes.iter().any(|k| reads.contains(k)) || (e.reads_coinbase && *credit)
                })
                .map(|(_, _, d)| *d)
                .max()
                .unwrap_or(0);
            depth.push((
                e.writes.iter().map(|(k, _)| *k).collect(),
                !e.credit.is_zero() || !e.debit.is_zero(),
                d,
            ));
        }
        report.chain = depth.iter().map(|(_, _, d)| *d).max().unwrap_or(0);
    }
    report.delta = dirty.len();
    report.dirty = dirty;
    report
}

struct Slot {
    number: u64,
    /// Earlier bases by builder (coinbase) and the pubkey that submitted them.
    priors: Vec<(alloy_primitives::Address, [u8; 48], Arc<Prior>)>,
    known: Known,
    /// Each builder's state trie at its last base.
    layers: FxHashMap<(alloy_primitives::Address, [u8; 48]), crate::engine::btrie::State>,
    /// The slot's system-call writes (beacon root, block hash history), the same for every base.
    system: Option<Vec<ethrex_common::types::AccountUpdate>>,
}

/// Adds an effect seen outside base replay (presim, order apply) to what B may reuse.
pub fn learn(effect: Effect) {
    if let Ok(mut guard) = SLOT.lock() {
        let slot = guard.get_or_insert_with(|| Slot::new(0));
        slot.known.ever.insert(variant_id(&effect));
        let variants = slot.known.effects.entry(effect.hash).or_default();
        if !variants.iter().any(|v| v.reads == effect.reads) {
            variants.insert(0, Arc::new(effect));
            variants.truncate(VARIANTS);
        }
    }
}

/// Records the slot's system-call writes the first time a base replays them.
pub fn set_system(updates: Vec<ethrex_common::types::AccountUpdate>, number: u64) {
    if let Ok(mut guard) = SLOT.lock() {
        let slot = Slot::at(&mut guard, number);
        if slot.system.is_none() {
            let mut system = FxHashMap::default();
            for update in &updates {
                for (key, value) in &update.added_storage {
                    system.insert(Key::Slot(update.address, *key), Val::Slot(*value));
                }
            }
            slot.known.system = Arc::new(system);
            slot.system = Some(updates);
        }
    }
}

impl Slot {
    fn new(number: u64) -> Self {
        Slot {
            number,
            priors: Vec::new(),
            known: Known::default(),
            layers: FxHashMap::default(),
            system: None,
        }
    }

    /// The state for slot `number`, dropping an earlier slot's.
    fn at(guard: &mut Option<Slot>, number: u64) -> &mut Slot {
        if guard.as_ref().is_some_and(|slot| slot.number != number) {
            *guard = None;
        }
        guard.get_or_insert_with(|| Slot::new(number))
    }
}

/// Turns final values (`None`: back to the parent's) into trie updates, with the coinbase's
/// balance and nonce from the base's running totals.
fn updates(
    values: impl Iterator<Item = (Key, Option<Val>)>,
    exec: &Exec,
    (credit, debit, coinbase_nonce): (U256, U256, Option<u64>),
) -> Result<Vec<ethrex_common::types::AccountUpdate>, DatabaseError> {
    use ethrex_common::types::{AccountInfo, AccountUpdate};
    let mut by_address: std::collections::BTreeMap<Address, AccountUpdate> = Default::default();
    let entry = |m: &mut std::collections::BTreeMap<Address, AccountUpdate>, a: Address| {
        m.entry(a).or_insert_with(|| AccountUpdate::new(a)).address
    };
    for (key, val) in values {
        match key {
            Key::Account(address) => {
                entry(&mut by_address, address);
                let update =
                    by_address.get_mut(&address).ok_or(DatabaseError::Custom("missing".into()))?;
                match val {
                    Some(Val::Account { exists: false, .. }) => update.removed = true,
                    Some(Val::Account { balance, nonce, code_hash, .. }) => {
                        update.info = Some(AccountInfo { code_hash, balance, nonce })
                    }
                    Some(Val::Slot(_)) => {
                        return Err(DatabaseError::Custom("slot value on account".into()))
                    }
                    None => {
                        let state = exec.store.get_account_state(address)?;
                        if state == AccountState::default() {
                            update.removed = true;
                        } else {
                            update.info = Some(AccountInfo {
                                code_hash: state.code_hash,
                                balance: state.balance,
                                nonce: state.nonce,
                            });
                        }
                    }
                }
            }
            Key::Slot(address, slot) => {
                entry(&mut by_address, address);
                let value = match val {
                    Some(Val::Slot(v)) => v,
                    Some(Val::Account { .. }) => {
                        return Err(DatabaseError::Custom("account value on slot".into()))
                    }
                    None => exec.store.get_storage_value(address, slot)?,
                };
                if let Some(update) = by_address.get_mut(&address) {
                    update.added_storage.insert(slot, value);
                }
            }
        }
    }
    let parent = exec.store.get_account_state(exec.coinbase)?;
    let coinbase =
        by_address.entry(exec.coinbase).or_insert_with(|| AccountUpdate::new(exec.coinbase));
    coinbase.info = Some(AccountInfo {
        code_hash: parent.code_hash,
        balance: parent.balance.saturating_add(credit).saturating_sub(debit),
        nonce: coinbase_nonce.unwrap_or(parent.nonce),
    });
    Ok(by_address.into_values().collect())
}

static SLOT: Mutex<Option<Slot>> = Mutex::new(None);

type Job = Box<dyn FnOnce() + Send>;

static WORKER: std::sync::LazyLock<crossbeam_channel::Sender<Job>> =
    std::sync::LazyLock::new(|| {
        let (sender, receiver) = crossbeam_channel::bounded::<Job>(4096);
        let _unused = std::thread::Builder::new()
            .name("b-shadow".into())
            .spawn(move || receiver.into_iter().for_each(|job| job()));
        sender
    });

/// Jobs dropped because the shadow thread fell behind.
pub static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs `job` on B's shadow thread, off the merge path.
pub fn defer(job: impl FnOnce() + Send + 'static) {
    if WORKER.try_send(Box::new(job)).is_err() {
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Waits for every job deferred so far.
pub fn drain() {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    if WORKER
        .send(Box::new(move || {
            let _unused = sender.send(());
        }))
        .is_ok()
    {
        let _unused = receiver.recv();
    }
}

/// Runs B against the builder's last base and against the earlier base that best aligns with
/// this one, then keeps this base for later ones.
pub fn shadow(
    builder: alloy_primitives::Address,
    effects: Vec<Option<Arc<Effect>>>,
    mut exec: Exec,
) -> Option<Report> {
    if effects.iter().any(Option::is_none) {
        // Not diffable, but every effect it does have is still worth knowing.
        for effect in effects.into_iter().flatten() {
            learn((*effect).clone());
        }
        return None;
    }
    let effects: Vec<Arc<Effect>> = effects.into_iter().flatten().collect();
    let mut guard = SLOT.lock().ok()?;
    let slot = Slot::at(&mut guard, exec.slot);
    for effect in &effects {
        if !effect.codes.is_empty() {
            let codes = Arc::make_mut(&mut slot.known.codes);
            for code in &effect.codes {
                codes.insert(code.hash, code.clone());
            }
        }
    }
    // Each pubkey is its own block stream; with `B_BY_PUBKEY` a base diffs against the last one
    // from its own stream, else against the builder's last base from any of its pubkeys.
    let by_pubkey = std::env::var_os("B_BY_PUBKEY").is_some();
    let key = (builder, if by_pubkey { exec.pubkey } else { [0; 48] });
    let last_any = slot.priors.iter().rev().find(|(b, _, _)| *b == builder);
    let last = if by_pubkey {
        slot.priors.iter().rev().find(|(b, k, _)| *b == builder && *k == exec.pubkey).or(last_any)
    } else {
        last_any
    };
    let same_stream = last_any.map(|(_, k, _)| *k == exec.pubkey);
    let same_stream_prior =
        slot.priors.iter().rev().any(|(b, k, _)| *b == builder && *k == exec.pubkey);
    let report = last.map(|(_, _, prior)| diff(prior, &slot.known, &effects, &exec));
    // Heavy bases: what the best earlier base of any builder or pubkey would have needed.
    let best_any = report.as_ref().filter(|r| r.run > 30).map(|_| {
        let mut seen = rustc_hash::FxHashSet::default();
        slot.priors
            .iter()
            .rev()
            .filter(|(b, k, _)| seen.insert((*b, *k)))
            .take(24)
            .map(|(_, _, prior)| diff(prior, &slot.known, &effects, &exec).run)
            .min()
            .unwrap_or(usize::MAX)
    });
    let mut report = report.unwrap_or_else(|| Report { first: true, ..Default::default() });
    if let (Some((_, _, prior)), Some(template)) = (last, exec.template.take()) {
        let ctx_start = Instant::now();
        report.ctx = prior.context(template, &report, &exec).ok();
        report.ctx_micros = ctx_start.elapsed().as_micros() as u64;
    }
    report.same_stream = same_stream;
    report.best_any = best_any;
    report.stream_first = !same_stream_prior;
    report.arrival_ms = TIMELINE.lock().ok().and_then(|t| {
        let at = *t.as_ref()?.1.get(&exec.base_hash)? as i64;
        Some(at / 1_000_000 - (MAINNET_GENESIS + exec.slot as i64 * 12) * 1000)
    });
    let body = &effects[..effects.len().saturating_sub(1)];
    let leads: Option<Vec<u64>> =
        body.iter().map(|e| lead(&e.hash, &exec.base_hash).flatten()).collect();
    report.content_lead = leads.and_then(|l| l.into_iter().min());
    let root_start = Instant::now();
    // A failed patch leaves the trie half-applied, so it is rebuilt from the parent instead.
    let layer_matches = !by_pubkey || same_stream_prior;
    let patched = match slot.layers.get_mut(&key).filter(|_| layer_matches) {
        Some(layer) if !report.first => updates(
            report.dirty.iter().map(|(k, v)| (*k, v.clone())),
            &exec,
            (report.credit, report.debit, report.coinbase_nonce),
        )
        .map_err(|e| e.to_string())
        .and_then(|ups| layer.apply(&ups).map_err(|e| e.to_string()))
        .ok(),
        _ => None,
    };
    report.rebuilt = patched.is_none();
    let root = match patched {
        Some(root) => Ok(root),
        None => {
            let mut finals: FxHashMap<Key, Val> = FxHashMap::default();
            let (mut credit, mut debit, mut nonce) = (U256::zero(), U256::zero(), None);
            for effect in &effects {
                for (key, val) in &effect.writes {
                    finals.insert(*key, val.clone());
                }
                credit = credit.saturating_add(effect.credit);
                debit = debit.saturating_add(effect.debit);
                nonce = effect.coinbase_nonce.or(nonce);
            }
            let built =
                crate::engine::state_layer::cached_state_db(&exec.trie_store, exec.parent_root)
                    .map_err(|e| e.to_string())
                    .and_then(|accounts| {
                        let (store, parent) = (exec.trie_store.clone(), exec.parent_root);
                        let mut layer = crate::engine::btrie::State::open(
                            accounts,
                            parent,
                            Box::new(move |hashed| {
                                crate::engine::state_layer::cached_storage_db(
                                    &store, hashed, parent,
                                )
                                .map_err(|e| ethrex_trie::TrieError::Verify(e.to_string()))
                            }),
                        );
                        layer
                            .apply(slot.system.as_deref().unwrap_or_default())
                            .map_err(|e| e.to_string())?;
                        let ups = updates(
                            finals.into_iter().map(|(k, v)| (k, Some(v))),
                            &exec,
                            (credit, debit, nonce),
                        )
                        .map_err(|e| e.to_string())?;
                        let root = layer.apply(&ups).map_err(|e| e.to_string())?;
                        Ok((layer, root))
                    });
            match built {
                Ok((layer, root)) => {
                    slot.layers.insert(key, layer);
                    Ok(root)
                }
                Err(e) => {
                    slot.layers.remove(&key);
                    Err(e)
                }
            }
        }
    };
    report.root_micros = root_start.elapsed().as_micros() as u64;
    report.root = root.ok();
    let report = Some(report);
    let prior = Arc::new(Prior::new(effects.clone(), &mut slot.known.parents));
    for effect in &effects {
        slot.known.ever.insert(variant_id(effect));
        let variants = slot.known.effects.entry(effect.hash).or_default();
        if !variants.iter().any(|v| v.reads == effect.reads) {
            variants.insert(0, effect.clone());
            variants.truncate(VARIANTS);
        }
    }
    slot.priors.push((builder, exec.pubkey, prior));
    report
}
