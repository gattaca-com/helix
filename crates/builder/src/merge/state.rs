//! Merge state as flat tables over slot-wide key ids. `Keys` interns each account and storage slot
//! the slot's merges touch, with its parent value, once per slot. Each merge's `State` holds the
//! values its block wrote over the parent, by id, with an undo log back to any earlier `At`.
//!
//! The EVM sees a `State` only inside `Vm::run`, which points the shared `View` at it for that
//! call. The run's changes are then written in by `State::fold`, which diffs levm's accounts
//! against their values as first read; a fresh levm database per run makes that diff the run's.
//!
//! Ids count from 1 (0 is unused). Past `MAX_ACCOUNTS` / `MAX_SLOTS` ids, or `MAX_UNDO` entries
//! since the last `forget`, interning and writing fail and the tx cannot go in.

use std::{cell::Cell, ptr, sync::Arc};

use ethrex_common::{
    Address, H256, U256,
    constants::{EMPTY_KECCAK_HASH, EMPTY_TRIE_HASH},
    types::{AccountState, ChainConfig, Code, CodeMetadata},
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::{
    account::{AccountStatus, LevmAccount},
    db::{
        Database,
        gen_db::{AccountSnapshot, GeneralizedDatabase},
    },
    errors::DatabaseError,
    precompiles::PrecompileCache,
    vm::VMType,
};
use ethrex_vm::Evm;
use rustc_hash::FxHashMap;

/// ~10x the most touched per slot measured: 2.2k accounts, 11.3k slots.
const MAX_ACCOUNTS: usize = 1 << 15;
const MAX_SLOTS: usize = 1 << 17;
/// Writes one order and the runs inside it can leave before the next `forget`.
const MAX_UNDO: usize = 1 << 14;

/// An account as a tx can observe it, short of storage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Acct {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: H256,
    pub has_storage: bool,
}

impl Acct {
    fn of_state(state: &AccountState) -> Acct {
        Acct {
            balance: state.balance,
            nonce: state.nonce,
            code_hash: state.code_hash,
            has_storage: state.storage_root != *EMPTY_TRIE_HASH,
        }
    }

    fn of(account: &LevmAccount) -> Acct {
        Acct {
            balance: account.info.balance,
            nonce: account.info.nonce,
            code_hash: account.info.code_hash,
            has_storage: account.has_storage,
        }
    }

    pub fn of_snapshot(snapshot: &AccountSnapshot) -> Acct {
        Acct {
            balance: snapshot.info.balance,
            nonce: snapshot.info.nonce,
            code_hash: snapshot.info.code_hash,
            has_storage: snapshot.has_storage,
        }
    }

    /// No balance, nonce or code. Absent from the state unless it holds storage, so levm, which
    /// derives `exists` from the state, sees the same.
    pub fn is_empty(&self) -> bool {
        self.balance.is_zero() && self.nonce == 0 && self.code_hash == *EMPTY_KECCAK_HASH
    }

    /// Everything a tx sees of the account, short of `BALANCE`, is fixed: it exists, is non-empty
    /// and runs no code.
    pub fn plain(&self) -> bool {
        self.code_hash == *EMPTY_KECCAK_HASH && !self.is_empty()
    }

    /// levm reads only whether the storage root is empty.
    fn state(&self) -> AccountState {
        AccountState {
            nonce: self.nonce,
            balance: self.balance,
            storage_root: if self.has_storage { H256::zero() } else { *EMPTY_TRIE_HASH },
            code_hash: self.code_hash,
        }
    }
}

/// Every key the slot's merges touched, with its parent value. Owned by the slot's `Parent`.
pub struct Keys {
    db: Arc<dyn Database>,
    account_ids: FxHashMap<Address, u32>,
    slot_ids: FxHashMap<(Address, H256), u32>,
    accounts: Vec<(Address, Acct)>,
    slots: Vec<(Address, H256, U256)>,
    /// Code the slot's runs deployed, for later runs to load.
    codes: FxHashMap<H256, Code>,
}

impl Keys {
    pub fn new(db: Arc<dyn Database>) -> Keys {
        let mut accounts = Vec::with_capacity(MAX_ACCOUNTS);
        accounts.push((Address::zero(), Acct::default()));
        let mut slots = Vec::with_capacity(MAX_SLOTS);
        slots.push((Address::zero(), H256::zero(), U256::zero()));
        Keys {
            db,
            account_ids: FxHashMap::with_capacity_and_hasher(MAX_ACCOUNTS, Default::default()),
            slot_ids: FxHashMap::with_capacity_and_hasher(MAX_SLOTS, Default::default()),
            accounts,
            slots,
            codes: FxHashMap::default(),
        }
    }

    pub fn account(&mut self, address: Address) -> Result<u32, &'static str> {
        if let Some(&id) = self.account_ids.get(&address) {
            return Ok(id);
        }
        if self.accounts.len() == MAX_ACCOUNTS {
            return Err("key_space_full");
        }
        let state = self.db.get_account_state(address).map_err(|_| "state_read")?;
        let id = self.accounts.len() as u32;
        self.accounts.push((address, Acct::of_state(&state)));
        self.account_ids.insert(address, id);
        Ok(id)
    }

    pub fn slot(&mut self, address: Address, key: H256) -> Result<u32, &'static str> {
        if let Some(&id) = self.slot_ids.get(&(address, key)) {
            return Ok(id);
        }
        if self.slots.len() == MAX_SLOTS {
            return Err("key_space_full");
        }
        let value = self.db.get_storage_value(address, key).map_err(|_| "state_read")?;
        let id = self.slots.len() as u32;
        self.slots.push((address, key, value));
        self.slot_ids.insert((address, key), id);
        Ok(id)
    }

    pub fn account_id(&self, address: Address) -> Option<u32> {
        self.account_ids.get(&address).copied()
    }

    /// Account `id`'s address and parent value.
    pub fn account_at(&self, id: u32) -> (Address, Acct) {
        debug_assert!(id != 0 && (id as usize) < self.accounts.len());
        self.accounts[id as usize]
    }

    /// Slot `id`'s address, key and parent value.
    pub fn slot_at(&self, id: u32) -> (Address, H256, U256) {
        debug_assert!(id != 0 && (id as usize) < self.slots.len());
        self.slots[id as usize]
    }
}

/// Values written over the parent by id, the ids in first-write order, and an undo log.
struct Table<T> {
    /// By id: written, and the value.
    vals: Vec<(bool, T)>,
    written: Vec<u32>,
    /// Id, value before, and whether it was the id's first write.
    undo: Vec<(u32, T, bool)>,
}

impl<T: Copy + Default> Table<T> {
    fn new(ids: usize) -> Table<T> {
        Table {
            vals: Vec::with_capacity(ids),
            written: Vec::with_capacity(ids),
            undo: Vec::with_capacity(MAX_UNDO),
        }
    }

    fn get(&self, id: u32) -> Option<T> {
        self.vals.get(id as usize).filter(|v| v.0).map(|v| v.1)
    }

    /// False, with nothing written, when the undo log is full.
    fn set(&mut self, id: u32, value: T) -> bool {
        if self.undo.len() == MAX_UNDO {
            return false;
        }
        let i = id as usize;
        if i >= self.vals.len() {
            // Within capacity: `Keys` hands out no id past it.
            debug_assert!(i < self.vals.capacity());
            self.vals.resize(i + 1, (false, T::default()));
        }
        let (set, old) = self.vals[i];
        self.undo.push((id, old, !set));
        if !set {
            self.written.push(id);
        }
        self.vals[i] = (true, value);
        true
    }

    fn undo_to(&mut self, len: u32) {
        while self.undo.len() > len as usize {
            let Some((id, old, first)) = self.undo.pop() else { break };
            let val = &mut self.vals[id as usize];
            if first {
                val.0 = false;
                // First writes since the mark are the newest in `written`.
                let last = self.written.pop();
                debug_assert_eq!(last, Some(id));
            } else {
                val.1 = old;
            }
        }
    }

    fn reset(&mut self) {
        for &id in &self.written {
            self.vals[id as usize].0 = false;
        }
        self.written.clear();
        self.undo.clear();
    }

    /// Ids whose value here differs from `held`'s, each once, with the value here. An id either
    /// has not written holds `parent(id)`.
    fn unlike<'a>(
        &'a self,
        held: &'a Table<T>,
        parent: impl Fn(u32) -> T + Copy + 'a,
    ) -> impl Iterator<Item = (u32, T)> + 'a
    where
        T: PartialEq,
    {
        let ours = self.written.iter().filter_map(move |&id| {
            let value = self.vals[id as usize].1;
            (held.get(id).unwrap_or_else(|| parent(id)) != value).then_some((id, value))
        });
        let theirs = held.written.iter().filter_map(move |&id| {
            let value = parent(id);
            (self.get(id).is_none() && held.vals[id as usize].1 != value).then_some((id, value))
        });
        ours.chain(theirs)
    }

    /// Becomes `src`'s written values, with an empty undo log.
    fn copy_from(&mut self, src: &Table<T>) {
        self.reset();
        if self.vals.len() < src.vals.len() {
            debug_assert!(src.vals.len() <= self.vals.capacity());
            self.vals.resize(src.vals.len(), (false, T::default()));
        }
        for &id in &src.written {
            self.vals[id as usize] = (true, src.vals[id as usize].1);
        }
        self.written.extend_from_slice(&src.written);
    }
}

/// A point in a `State`'s undo log.
#[derive(Clone, Copy)]
pub struct At {
    accounts: u32,
    slots: u32,
}

/// One merge's block state over the parent.
pub struct State {
    accounts: Table<Acct>,
    slots: Table<U256>,
}

impl Default for State {
    fn default() -> State {
        State { accounts: Table::new(MAX_ACCOUNTS), slots: Table::new(MAX_SLOTS) }
    }
}

impl State {
    pub fn account(&self, keys: &Keys, id: u32) -> Acct {
        self.accounts.get(id).unwrap_or_else(|| keys.account_at(id).1)
    }

    pub fn slot(&self, keys: &Keys, id: u32) -> U256 {
        self.slots.get(id).unwrap_or_else(|| keys.slot_at(id).2)
    }

    /// False, with nothing written, when the undo log is full.
    pub fn set_account(&mut self, id: u32, value: Acct) -> bool {
        self.accounts.set(id, value)
    }

    pub fn set_slot(&mut self, id: u32, value: U256) -> bool {
        self.slots.set(id, value)
    }

    pub fn at(&self) -> At {
        At { accounts: self.accounts.undo.len() as u32, slots: self.slots.undo.len() as u32 }
    }

    pub fn undo_to(&mut self, at: At) {
        self.accounts.undo_to(at.accounts);
        self.slots.undo_to(at.slots);
    }

    /// Makes everything written so far permanent until `reset`; every `At` taken before is void.
    pub fn forget(&mut self) {
        self.accounts.undo.clear();
        self.slots.undo.clear();
    }

    /// Back to the parent.
    pub fn reset(&mut self) {
        self.accounts.reset();
        self.slots.reset();
    }

    /// Account ids written since `at`; each once when one run or replay wrote them.
    pub fn accounts_since(&self, at: At) -> impl Iterator<Item = u32> + '_ {
        self.accounts.undo[at.accounts as usize..].iter().map(|u| u.0)
    }

    pub fn slots_since(&self, at: At) -> impl Iterator<Item = u32> + '_ {
        self.slots.undo[at.slots as usize..].iter().map(|u| u.0)
    }

    /// Writes what the run left in `db`: each changed account and each slot that differs from its
    /// value as first read. On `Err` part may be written; undo to an `At` from before.
    pub fn fold(&mut self, keys: &mut Keys, db: &GeneralizedDatabase) -> Result<(), &'static str> {
        for (&address, account) in &db.current_accounts_state {
            if account.is_unmodified() {
                continue;
            }
            let initial = db.initial_accounts_state.get(&address).ok_or("state_fold")?;
            let destroyed = matches!(
                account.status,
                AccountStatus::Destroyed | AccountStatus::DestroyedModified
            );
            // Wiping parent storage needs every slot of it; post-Cancun only an account created
            // in the same tx is destroyed, and that has none.
            if destroyed && initial.has_storage {
                return Err("destroyed_storage");
            }
            let now = Acct::of(account);
            if now != Acct::of(initial) {
                if now.code_hash != initial.info.code_hash &&
                    !keys.codes.contains_key(&now.code_hash)
                {
                    let code = db.codes.get(&now.code_hash).ok_or("state_fold")?;
                    keys.codes.insert(now.code_hash, code.clone());
                }
                let id = keys.account(address)?;
                if !self.accounts.set(id, now) {
                    return Err("state_full");
                }
            }
            for (&key, &value) in &account.storage {
                let old = match destroyed {
                    true => U256::zero(),
                    false => *initial.storage.get(&key).ok_or("state_fold")?,
                };
                if value == old {
                    continue;
                }
                let id = keys.slot(address, key)?;
                if !self.slots.set(id, value) {
                    return Err("state_full");
                }
            }
        }
        Ok(())
    }

    /// Accounts whose value here differs from `held`'s, with the value here.
    pub fn accounts_unlike<'a>(
        &'a self,
        held: &'a State,
        keys: &'a Keys,
    ) -> impl Iterator<Item = (u32, Acct)> + 'a {
        self.accounts.unlike(&held.accounts, move |id| keys.account_at(id).1)
    }

    pub fn slots_unlike<'a>(
        &'a self,
        held: &'a State,
        keys: &'a Keys,
    ) -> impl Iterator<Item = (u32, U256)> + 'a {
        self.slots.unlike(&held.slots, move |id| keys.slot_at(id).2)
    }

    /// Becomes `src` as it stands, with nothing to undo.
    pub fn copy_from(&mut self, src: &State) {
        self.accounts.copy_from(&src.accounts);
        self.slots.copy_from(&src.slots);
    }
}

/// The EVM's database inside `Vm::run`: the parent with one `State` over it. Null outside a run.
struct View {
    keys: Cell<*const Keys>,
    state: Cell<*const State>,
}

// SAFETY: levm calls in only from the engine thread, inside `Vm::run`.
unsafe impl Send for View {}
unsafe impl Sync for View {}

impl View {
    fn get(&self) -> (&Keys, &State) {
        let (keys, state) = (self.keys.get(), self.state.get());
        assert!(!keys.is_null() && !state.is_null(), "merge view used outside a run");
        // SAFETY: `Vm::run` sets both from references that outlive the run, and nulls them
        // before it returns.
        unsafe { (&*keys, &*state) }
    }
}

impl Database for View {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        let (keys, state) = self.get();
        match keys.account_ids.get(&address) {
            Some(&id) => Ok(state.account(keys, id).state()),
            None => keys.db.get_account_state(address),
        }
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        let (keys, state) = self.get();
        match keys.slot_ids.get(&(address, key)) {
            Some(&id) => Ok(state.slot(keys, id)),
            None => keys.db.get_storage_value(address, key),
        }
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        self.get().0.db.get_block_hash(block_number)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.get().0.db.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        let keys = self.get().0;
        match keys.codes.get(&code_hash) {
            Some(code) => Ok(code.clone()),
            None => keys.db.get_account_code(code_hash),
        }
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        let keys = self.get().0;
        match keys.codes.get(&code_hash) {
            Some(code) => Ok(CodeMetadata { length: code.len() as u64 }),
            None => keys.db.get_code_metadata(code_hash),
        }
    }

    fn precompile_cache(&self) -> Option<&PrecompileCache> {
        self.get().0.db.precompile_cache()
    }
}

/// The engine thread's one EVM. Its database is emptied before each run, so it holds only that
/// run's accounts.
pub struct Vm {
    pub evm: Evm,
    view: Arc<View>,
}

impl Default for Vm {
    fn default() -> Vm {
        let view = Arc::new(View { keys: Cell::new(ptr::null()), state: Cell::new(ptr::null()) });
        let evm = Evm {
            db: GeneralizedDatabase::new(view.clone()),
            vm_type: VMType::L1,
            crypto: Arc::new(NativeCrypto),
            stateless_validator: None,
        };
        Vm { evm, view }
    }
}

impl Vm {
    /// Runs `f` with the EVM reading `state` over the parent in `keys`. What `f` leaves in
    /// `evm.db` stays readable until the next run, for `State::fold`.
    pub fn run<R>(&mut self, keys: &Keys, state: &State, f: impl FnOnce(&mut Evm) -> R) -> R {
        struct Clear<'a>(&'a View);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.keys.set(ptr::null());
                self.0.state.set(ptr::null());
            }
        }
        let db = &mut self.evm.db;
        db.current_accounts_state.clear();
        db.initial_accounts_state.clear();
        db.codes.clear();
        db.code_metadata.clear();
        db.tx_backup = None;
        db.tx_reads = None;
        self.view.keys.set(keys);
        self.view.state.set(state);
        let _clear = Clear(&self.view);
        f(&mut self.evm)
    }
}

#[cfg(test)]
mod tests {
    use ethrex_common::U256;

    use super::Table;

    #[test]
    fn undo_restores_values_and_written() {
        let mut t = Table::<U256>::new(16);
        assert!(t.set(3, U256::from(1)));
        let at = t.undo.len() as u32;
        assert!(t.set(3, U256::from(2)));
        assert!(t.set(5, U256::from(7)));
        assert_eq!((t.get(3), t.get(5)), (Some(U256::from(2)), Some(U256::from(7))));
        t.undo_to(at);
        assert_eq!((t.get(3), t.get(5), &t.written[..]), (Some(U256::from(1)), None, &[3][..]));
        t.undo.clear();
        assert!(t.set(5, U256::from(8)));
        t.undo_to(0);
        assert_eq!((t.get(5), &t.written[..]), (None, &[3][..]));
        t.reset();
        assert_eq!((t.get(3), t.written.len(), t.undo.len()), (None, 0, 0));
    }
}
