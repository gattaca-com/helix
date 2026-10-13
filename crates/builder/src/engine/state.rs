//! B's state: every tx kept as its effect, what it read and what it wrote. A builder's new base is
//! its previous base with only the txs whose reads changed run again; orders go on top the same
//! way.

use std::{
    cell::RefCell,
    sync::{Arc, OnceLock},
    time::Instant,
};

use alloy_primitives::B256;
use ethrex_common::{
    Address, Bloom, H256, U256,
    constants::EMPTY_TRIE_HASH,
    types::{
        AccountInfo, AccountState, AccountUpdate, BlockHeader, ChainConfig, Code, CodeMetadata,
        Log, TxType, bloom_from_logs,
    },
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::{
    db::{
        Database,
        gen_db::{AccountSnapshot, GeneralizedDatabase, TxReads},
    },
    errors::DatabaseError,
    precompiles::PrecompileCache,
    vm::VMType,
};
use ethrex_rlp::encode::{RLPEncode, encode_length};
use ethrex_vm::Evm;
use rustc_hash::FxHashMap;

use crate::engine::types::DecodedTx;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Key {
    Account(Address),
    Slot(Address, H256),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Val {
    Account { balance: U256, nonce: u64, code_hash: H256, exists: bool, has_storage: bool },
    Slot(U256),
}

impl Val {
    fn of(snapshot: &AccountSnapshot) -> Self {
        Val::Account {
            balance: snapshot.info.balance,
            nonce: snapshot.info.nonce,
            code_hash: snapshot.info.code_hash,
            exists: snapshot.exists,
            has_storage: snapshot.has_storage,
        }
    }

    /// An account as the parent state holds it.
    fn of_state(state: &AccountState) -> Self {
        Val::Account {
            balance: state.balance,
            nonce: state.nonce,
            code_hash: state.code_hash,
            exists: *state != AccountState::default(),
            has_storage: state.storage_root != *EMPTY_TRIE_HASH,
        }
    }

    pub fn balance(&self) -> U256 {
        match self {
            Val::Account { balance, .. } => *balance,
            Val::Slot(_) => U256::zero(),
        }
    }
}

/// What one tx read (first value seen) and wrote (final value), coinbase excluded.
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
    /// The receipt's bloom, and its bloom and logs as RLP: the bulk of the receipt, the same
    /// wherever the effect lands.
    encoded: OnceLock<(Bloom, Vec<u8>)>,
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
    fn from_db(
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
            encoded: OnceLock::new(),
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
            if after.info.code_hash != code_hash(&before) {
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
        out.reads.sort_by_key(|(k, _)| *k);
        out.writes.sort_by_key(|(k, _)| *k);
        Some(out)
    }
}

impl Effect {
    /// The tx's receipt as the receipt trie holds it at `cumulative` receipt gas, and its bloom.
    pub fn receipt(&self, tx_type: TxType, cumulative: u64) -> (Bloom, Vec<u8>) {
        let (bloom, tail) = self.encoded.get_or_init(|| {
            let bloom = bloom_from_logs(&self.outcome.logs, &NativeCrypto);
            let mut tail = Vec::new();
            bloom.encode(&mut tail);
            self.outcome.logs.encode(&mut tail);
            (bloom, tail)
        });
        let mut head = Vec::with_capacity(16);
        self.outcome.succeeded.encode(&mut head);
        cumulative.encode(&mut head);
        let mut out = Vec::with_capacity(head.len() + tail.len() + 10);
        if tx_type != TxType::Legacy {
            out.push(tx_type as u8);
        }
        encode_length(head.len() + tail.len(), &mut out);
        out.extend_from_slice(&head);
        out.extend_from_slice(tail);
        (*bloom, out)
    }
}

fn code_hash(val: &Val) -> H256 {
    match val {
        Val::Account { code_hash, .. } => *code_hash,
        Val::Slot(_) => H256::zero(),
    }
}

/// The coinbase as a run stands at some tx.
#[derive(Clone, Copy)]
pub struct Coinbase {
    pub address: Address,
    pub balance: U256,
    pub nonce: u64,
}

impl Coinbase {
    /// Whether `effect`'s use of the coinbase still holds here. Under another coinbase it does only
    /// if it never involved this one, since its reads and writes leave the coinbase out. Under
    /// this one a tx that read the coinbase needs the same nonce, an uninspected balance, and at
    /// least the balance it found.
    pub fn admits(&self, effect: &Effect) -> bool {
        if effect.coinbase != self.address {
            let mine = Key::Account(self.address);
            return !(effect.reads_coinbase ||
                effect.coinbase_address ||
                effect.reads.iter().any(|(k, _)| *k == mine) ||
                effect.writes.iter().any(|(k, _)| *k == mine));
        }
        match effect.coinbase_seen {
            _ if !effect.reads_coinbase => true,
            Some((nonce, balance, inspected)) => {
                !inspected && nonce == self.nonce && self.balance >= balance
            }
            None => false,
        }
    }

    /// The coinbase after `effect`.
    pub fn after(self, effect: &Effect) -> Self {
        Coinbase {
            address: self.address,
            balance: self.balance.saturating_add(effect.credit).saturating_sub(effect.debit),
            nonce: effect.coinbase_nonce.unwrap_or(self.nonce),
        }
    }
}

/// A base as B applied it: its effects in order and every write per key, for the builder's next
/// base to diff against and for reads of the base's final state.
pub struct Prior {
    pub base: B256,
    pub effects: Vec<Arc<Effect>>,
    position: FxHashMap<B256, usize>,
    /// Every write per key in tx order.
    history: FxHashMap<Key, Vec<(usize, Val)>>,
}

impl Prior {
    pub fn new(base: B256, effects: Vec<Arc<Effect>>, known: &mut Known) -> Self {
        let mut history: FxHashMap<Key, Vec<(usize, Val)>> = FxHashMap::default();
        for (ix, effect) in effects.iter().enumerate() {
            for (key, val) in &effect.reads {
                if !history.contains_key(key) {
                    known.parents.entry(*key).or_insert_with(|| val.clone());
                }
            }
            for (key, val) in &effect.writes {
                history.entry(*key).or_default().push((ix, val.clone()));
            }
        }
        let position = effects.iter().enumerate().map(|(ix, e)| (e.hash, ix)).collect();
        Prior { base, effects, position, history }
    }

    /// The key's value in this run just before tx `at`; `None` when unwritten so far.
    fn before(&self, key: &Key, at: usize) -> Option<&Val> {
        let writes = self.history.get(key)?;
        let n = writes.partition_point(|(ix, _)| *ix < at);
        n.checked_sub(1).map(|n| &writes[n].1)
    }

    /// The key's value at the end of the run; `None` when the run never wrote it.
    pub fn last(&self, key: &Key) -> Option<&Val> {
        self.before(key, self.effects.len())
    }

    /// Every key the run wrote.
    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.history.keys()
    }

    /// Common txs kept in relative order: per new tx, its position here when on that path.
    fn align(&self, new: &[Arc<DecodedTx>]) -> Vec<Option<usize>> {
        let olds: Vec<Option<usize>> =
            new.iter().map(|tx| self.position.get(&tx.hash).copied()).collect();
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

const VARIANTS: usize = 16;

/// Slot-wide knowledge: parent values seen in reads, every effect seen per tx, and deployed code.
#[derive(Default)]
pub struct Known {
    parents: FxHashMap<Key, Val>,
    effects: FxHashMap<B256, Vec<Arc<Effect>>>,
    pub codes: Arc<FxHashMap<H256, Code>>,
}

impl Known {
    /// Keeps `effect` among its tx's variants, and the code it deployed.
    pub fn add(&mut self, effect: Arc<Effect>) {
        if !effect.codes.is_empty() {
            let codes = Arc::make_mut(&mut self.codes);
            for code in &effect.codes {
                codes.insert(code.hash, code.clone());
            }
        }
        let variants = self.effects.entry(effect.hash).or_default();
        if !variants.iter().any(|v| v.reads == effect.reads) {
            variants.insert(0, effect);
            variants.truncate(VARIANTS);
        }
    }

    pub fn variants(&self, hash: &B256) -> impl Iterator<Item = &Arc<Effect>> {
        self.effects.get(hash).into_iter().flatten()
    }
}

/// What B needs to run a base's txs: the txs, the header they run under, the parent state, and
/// whom the base's last tx pays.
pub struct Exec {
    pub txs: Arc<Vec<Arc<DecodedTx>>>,
    pub header: BlockHeader,
    pub store: Arc<dyn Database>,
    pub coinbase: Address,
    pub proposer: Address,
    /// The storage the slot's system calls wrote before any tx.
    pub system: Arc<FxHashMap<Key, Val>>,
}

/// A run's state just before one tx: the innermost layer holding a key, else `prior`'s run at
/// `at`, else the slot's system writes, else the parent.
#[derive(Clone)]
pub struct View {
    pub prior: Arc<Prior>,
    pub at: usize,
    /// Values over `prior`'s, innermost last; `None` is back to the parent's.
    pub layers: Vec<Arc<FxHashMap<Key, Option<Val>>>>,
    pub coinbase: Coinbase,
    pub codes: Arc<FxHashMap<H256, Code>>,
    pub system: Arc<FxHashMap<Key, Val>>,
    pub store: Arc<dyn Database>,
}

impl View {
    /// The key's value above the parent; `None` when the run holds the parent's.
    fn val(&self, key: &Key) -> Option<Val> {
        for layer in self.layers.iter().rev() {
            if let Some(val) = layer.get(key) {
                return val.clone();
            }
        }
        self.prior.before(key, self.at).cloned()
    }

    /// The key's value, through to the parent state.
    pub fn value(&self, key: &Key) -> Option<Val> {
        self.val(key).or_else(|| self.system.get(key).cloned()).or_else(|| match key {
            Key::Account(address) => {
                self.store.get_account_state(*address).ok().map(|state| Val::of_state(&state))
            }
            Key::Slot(address, slot) => {
                self.store.get_storage_value(*address, *slot).ok().map(Val::Slot)
            }
        })
    }

    /// Whether `effect` still holds here: its reads are the values here and its use of the
    /// coinbase fits.
    pub fn holds(&self, effect: &Effect) -> bool {
        self.coinbase.admits(effect) &&
            effect.reads.iter().all(|(key, val)| self.value(key).as_ref() == Some(val))
    }
}

impl Database for View {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        if address == self.coinbase.address {
            let mut state = self.store.get_account_state(address)?;
            state.balance = self.coinbase.balance;
            state.nonce = self.coinbase.nonce;
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

    fn precompile_cache(&self) -> Option<&PrecompileCache> {
        self.store.precompile_cache()
    }
}

/// Runs `tx` on `view` and keeps its effect; `None` when the tx is invalid there.
pub fn execute(view: View, tx: &DecodedTx, header: &BlockHeader) -> Option<Effect> {
    let coinbase = view.coinbase.address;
    let before = view.coinbase.balance;
    let mut vm = Evm {
        db: GeneralizedDatabase::new(Arc::new(view)),
        vm_type: VMType::L1,
        crypto: Arc::new(NativeCrypto),
        stateless_validator: None,
    };
    vm.db.tx_reads = Some(Default::default());
    let mut gas = 0;
    let (receipt, ran) = vm.execute_tx(&tx.tx, header, &mut gas, tx.sender).ok()?;
    vm.db.reads_paused = true;
    let after = vm.db.get_account(coinbase).ok()?.info.balance;
    let reads = vm.db.tx_reads.take()?;
    Effect::from_db(
        tx.hash,
        &vm.db,
        &reads,
        coinbase,
        (after.saturating_sub(before), before.saturating_sub(after)),
        Outcome {
            succeeded: receipt.succeeded,
            gas_used: ran.gas_used,
            gas_spent: ran.gas_spent,
            logs: receipt.logs,
        },
    )
}

/// How a base differs from the prior it was diffed against, and what B ran for it.
#[derive(Default)]
pub struct Report {
    /// Keys whose final value differs from the prior's final value; `None` is the parent's.
    pub dirty: FxHashMap<Key, Option<Val>>,
    /// The effect applied per tx, in base order.
    pub applied: Vec<Arc<Effect>>,
    /// The effects of the txs B ran itself.
    pub ran: Vec<Arc<Effect>>,
    pub warmed: Vec<Arc<Effect>>,
    /// The coinbase after the base.
    pub coinbase: Option<Coinbase>,
    /// The proposer's account just before the base's last tx.
    pub proposer_before: Option<Val>,
    pub micros: u64,
    pub exec_micros: u64,
}

/// Diffs the base in `exec` against `prior`: a tx is skipped when one of its known effects still
/// holds on B's state at that point, otherwise B runs it. `None` when a tx cannot run.
pub type Prewarmed = dashmap::DashMap<B256, Arc<Effect>, rustc_hash::FxBuildHasher>;

pub fn diff(
    prior: &Arc<Prior>,
    known: &Known,
    prewarmed: &Prewarmed,
    exec: &Exec,
) -> Option<Report> {
    let start = Instant::now();
    let old = &prior.effects;
    let aligned = prior.align(&exec.txs);
    // A parent value no earlier tx read comes from the parent store, through the slot's system
    // calls, and is kept for the rest of this base.
    let fetched: RefCell<FxHashMap<Key, Option<Val>>> = RefCell::new(FxHashMap::default());
    let parent = |key: &Key| -> Option<Val> {
        if let Some(v) = known.parents.get(key).or_else(|| exec.system.get(key)) {
            return Some(v.clone());
        }
        if let Some(v) = fetched.borrow().get(key) {
            return v.clone();
        }
        let v = match key {
            Key::Account(address) => {
                exec.store.get_account_state(*address).ok().map(|state| Val::of_state(&state))
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
    let parent_coinbase = exec.store.get_account_state(exec.coinbase).ok()?;
    let mut coinbase = Coinbase {
        address: exec.coinbase,
        balance: parent_coinbase.balance,
        nonce: parent_coinbase.nonce,
    };
    // Code deployed earlier in this base, for the txs B runs after it.
    let mut codes = known.codes.clone();
    let mut report = Report::default();

    for (i, tx) in exec.txs.iter().enumerate() {
        // Old txs before the aligned one ran only in the old sequence.
        if let Some(p) = aligned[i] {
            for effect in &old[optr..p] {
                for (key, _) in &effect.writes {
                    if !dirty.contains_key(key) {
                        let v = resolve(key, prior.before(key, optr));
                        dirty.insert(*key, v);
                    }
                }
            }
            optr = p;
        }
        let now = |dirty: &FxHashMap<Key, Option<Val>>, key: &Key| -> Option<Val> {
            match dirty.get(key) {
                Some(v) => v.clone(),
                None => resolve(key, prior.before(key, optr)),
            }
        };
        if i + 1 == exec.txs.len() {
            report.proposer_before = now(&dirty, &Key::Account(exec.proposer));
        }
        let holds = |e: &Effect, dirty: &FxHashMap<Key, Option<Val>>| {
            coinbase.admits(e) &&
                e.reads.iter().all(|(key, val)| now(dirty, key).as_ref() == Some(val))
        };
        // The aligned old effect holds unless a read is dirty, so check only those.
        let same_old = aligned[i].is_some_and(|p| {
            coinbase.admits(&old[p]) &&
                old[p]
                    .reads
                    .iter()
                    .all(|(key, val)| dirty.get(key).is_none_or(|now| now.as_ref() == Some(val)))
        });
        let warm = prewarmed.get(&tx.hash).map(|e| e.clone());
        let chosen: Option<&Arc<Effect>> = if same_old {
            aligned[i].map(|p| &old[p])
        } else {
            prior
                .position
                .get(&tx.hash)
                .map(|p| &old[*p])
                .into_iter()
                .chain(known.variants(&tx.hash))
                .chain(warm.as_ref())
                .find(|e| holds(e, &dirty))
        };
        let applied = match chosen {
            Some(effect) if warm.as_ref().is_some_and(|w| Arc::ptr_eq(w, effect)) => {
                report.warmed.push(effect.clone());
                effect.clone()
            }
            Some(effect) => effect.clone(),
            None => {
                let exec_start = Instant::now();
                let view = View {
                    prior: prior.clone(),
                    at: optr,
                    layers: vec![Arc::new(dirty.clone())],
                    coinbase,
                    codes: codes.clone(),
                    system: exec.system.clone(),
                    store: exec.store.clone(),
                };
                let own = Arc::new(execute(view, tx, &exec.header)?);
                report.exec_micros += exec_start.elapsed().as_micros() as u64;
                report.ran.push(own.clone());
                own
            }
        };
        if !applied.codes.is_empty() {
            let codes = Arc::make_mut(&mut codes);
            for code in &applied.codes {
                codes.insert(code.hash, code.clone());
            }
        }
        coinbase = coinbase.after(&applied);
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
    // Old txs after the last aligned one ran only in the old sequence too.
    for effect in &old[optr..] {
        for (key, _) in &effect.writes {
            if !dirty.contains_key(key) {
                let v = resolve(key, prior.before(key, optr));
                dirty.insert(*key, v);
            }
        }
    }
    report.micros = start.elapsed().as_micros() as u64;
    report.coinbase = Some(coinbase);
    report.dirty = dirty;
    Some(report)
}

/// Turns final values (`None`: back to the parent's) into trie updates, with the coinbase as
/// `coinbase` holds it.
pub fn updates(
    values: impl Iterator<Item = (Key, Option<Val>)>,
    store: &dyn Database,
    coinbase: Coinbase,
) -> Result<Vec<AccountUpdate>, DatabaseError> {
    let mut by_address: std::collections::BTreeMap<Address, AccountUpdate> = Default::default();
    for (key, val) in values {
        let address = match key {
            Key::Account(address) | Key::Slot(address, _) => address,
        };
        let update = by_address.entry(address).or_insert_with(|| AccountUpdate::new(address));
        match (key, val) {
            (Key::Account(_), Some(Val::Account { exists: false, .. })) => update.removed = true,
            (Key::Account(_), Some(Val::Account { balance, nonce, code_hash, .. })) => {
                update.info = Some(AccountInfo { code_hash, balance, nonce })
            }
            (Key::Account(_), None) => {
                let state = store.get_account_state(address)?;
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
            (Key::Slot(_, slot), Some(Val::Slot(value))) => {
                update.added_storage.insert(slot, value);
            }
            (Key::Slot(_, slot), None) => {
                update.added_storage.insert(slot, store.get_storage_value(address, slot)?);
            }
            _ => return Err(DatabaseError::Custom("key and value kinds differ".into())),
        }
    }
    let code_hash = store.get_account_state(coinbase.address)?.code_hash;
    let update =
        by_address.entry(coinbase.address).or_insert_with(|| AccountUpdate::new(coinbase.address));
    update.info = Some(AccountInfo { code_hash, balance: coinbase.balance, nonce: coinbase.nonce });
    Ok(by_address.into_values().collect())
}
