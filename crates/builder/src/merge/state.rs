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

// ~10x the most touched per slot measured: 2.2k accounts, 11.3k slots.
const MAX_ACCOUNTS: usize = 1 << 15;
const MAX_SLOTS: usize = 1 << 17;
const MAX_UNDO: usize = 1 << 14;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Account {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: H256,
    pub has_storage: bool,
}

impl Account {
    fn from_state(state: &AccountState) -> Account {
        Account {
            balance: state.balance,
            nonce: state.nonce,
            code_hash: state.code_hash,
            has_storage: state.storage_root != *EMPTY_TRIE_HASH,
        }
    }

    fn from_levm(account: &LevmAccount) -> Account {
        Account {
            balance: account.info.balance,
            nonce: account.info.nonce,
            code_hash: account.info.code_hash,
            has_storage: account.has_storage,
        }
    }

    pub fn from_snapshot(snapshot: &AccountSnapshot) -> Account {
        Account {
            balance: snapshot.info.balance,
            nonce: snapshot.info.nonce,
            code_hash: snapshot.info.code_hash,
            has_storage: snapshot.has_storage,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.balance.is_zero() && self.nonce == 0 && self.code_hash == *EMPTY_KECCAK_HASH
    }

    pub fn is_plain_eoa(&self) -> bool {
        self.code_hash == *EMPTY_KECCAK_HASH && !self.is_empty()
    }

    fn to_state(self) -> AccountState {
        AccountState {
            nonce: self.nonce,
            balance: self.balance,
            storage_root: if self.has_storage { H256::zero() } else { *EMPTY_TRIE_HASH },
            code_hash: self.code_hash,
        }
    }
}

pub struct KeyTable {
    db: Arc<dyn Database>,
    account_ids: FxHashMap<Address, u32>,
    slot_ids: FxHashMap<(Address, H256), u32>,
    accounts: Vec<(Address, Account)>,
    // Parallel to `accounts`; read only for the state root.
    storage_roots: Vec<H256>,
    slots: Vec<(Address, H256, U256)>,
    deployed_codes: FxHashMap<H256, Code>,
}

impl KeyTable {
    pub fn new(db: Arc<dyn Database>) -> KeyTable {
        let mut accounts = Vec::with_capacity(MAX_ACCOUNTS);
        // Id 0 is a sentinel.
        accounts.push((Address::zero(), Account::default()));
        let mut storage_roots = Vec::with_capacity(MAX_ACCOUNTS);
        storage_roots.push(H256::zero());
        let mut slots = Vec::with_capacity(MAX_SLOTS);
        slots.push((Address::zero(), H256::zero(), U256::zero()));
        KeyTable {
            db,
            account_ids: FxHashMap::with_capacity_and_hasher(MAX_ACCOUNTS, Default::default()),
            slot_ids: FxHashMap::with_capacity_and_hasher(MAX_SLOTS, Default::default()),
            accounts,
            storage_roots,
            slots,
            deployed_codes: FxHashMap::default(),
        }
    }

    pub fn reset(&mut self, db: Arc<dyn Database>) {
        self.db = db;
        self.account_ids.clear();
        self.slot_ids.clear();
        self.accounts.truncate(1);
        self.storage_roots.truncate(1);
        self.slots.truncate(1);
        self.deployed_codes.clear();
    }

    pub fn intern_account(&mut self, address: Address) -> Result<u32, &'static str> {
        if let Some(&id) = self.account_ids.get(&address) {
            return Ok(id);
        }
        if self.accounts.len() == MAX_ACCOUNTS {
            return Err("key_space_full");
        }
        let state = self.db.get_account_state(address).map_err(|_| "state_read")?;
        let id = self.accounts.len() as u32;
        self.accounts.push((address, Account::from_state(&state)));
        self.storage_roots.push(state.storage_root);
        self.account_ids.insert(address, id);
        Ok(id)
    }

    pub fn intern_slot(&mut self, address: Address, key: H256) -> Result<u32, &'static str> {
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

    pub fn find_account(&self, address: Address) -> Option<u32> {
        self.account_ids.get(&address).copied()
    }

    pub fn parent_account(&self, id: u32) -> (Address, Account) {
        debug_assert!(id != 0 && (id as usize) < self.accounts.len());
        self.accounts[id as usize]
    }

    pub fn parent_storage_root(&self, id: u32) -> H256 {
        debug_assert!(id != 0 && (id as usize) < self.storage_roots.len());
        self.storage_roots[id as usize]
    }

    pub fn parent_slot(&self, id: u32) -> (Address, H256, U256) {
        debug_assert!(id != 0 && (id as usize) < self.slots.len());
        self.slots[id as usize]
    }
}

pub(super) struct UndoTable<T> {
    values: Vec<(bool, T)>,
    pub(super) written: Vec<u32>,
    pub(super) undo_log: Vec<(u32, T, bool)>,
}

impl<T: Copy + Default> UndoTable<T> {
    pub(super) fn new(ids: usize) -> UndoTable<T> {
        UndoTable {
            values: Vec::with_capacity(ids),
            written: Vec::with_capacity(ids),
            undo_log: Vec::with_capacity(MAX_UNDO),
        }
    }

    pub(super) fn get(&self, id: u32) -> Option<T> {
        self.values.get(id as usize).filter(|v| v.0).map(|v| v.1)
    }

    pub(super) fn set(&mut self, id: u32, value: T) -> bool {
        if self.undo_log.len() == MAX_UNDO {
            return false;
        }
        let i = id as usize;
        if i >= self.values.len() {
            debug_assert!(i < self.values.capacity());
            self.values.resize(i + 1, (false, T::default()));
        }
        let (set, old) = self.values[i];
        self.undo_log.push((id, old, !set));
        if !set {
            self.written.push(id);
        }
        self.values[i] = (true, value);
        true
    }

    pub(super) fn undo_to(&mut self, len: u32) {
        while self.undo_log.len() > len as usize {
            let Some((id, old, first)) = self.undo_log.pop() else { break };
            let val = &mut self.values[id as usize];
            if first {
                val.0 = false;
                let last = self.written.pop();
                debug_assert_eq!(last, Some(id));
            } else {
                val.1 = old;
            }
        }
    }

    pub(super) fn reset(&mut self) {
        for &id in &self.written {
            self.values[id as usize].0 = false;
        }
        self.written.clear();
        self.undo_log.clear();
    }

    fn diff<'a>(
        &'a self,
        held: &'a UndoTable<T>,
        parent: impl Fn(u32) -> T + Copy + 'a,
    ) -> impl Iterator<Item = (u32, T)> + 'a
    where
        T: PartialEq,
    {
        let ours = self.written.iter().filter_map(move |&id| {
            let value = self.values[id as usize].1;
            (held.get(id).unwrap_or_else(|| parent(id)) != value).then_some((id, value))
        });
        let theirs = held.written.iter().filter_map(move |&id| {
            let value = parent(id);
            (self.get(id).is_none() && held.values[id as usize].1 != value).then_some((id, value))
        });
        ours.chain(theirs)
    }

    fn copy_from(&mut self, src: &UndoTable<T>) {
        self.reset();
        if self.values.len() < src.values.len() {
            debug_assert!(src.values.len() <= self.values.capacity());
            self.values.resize(src.values.len(), (false, T::default()));
        }
        for &id in &src.written {
            self.values[id as usize] = (true, src.values[id as usize].1);
        }
        self.written.extend_from_slice(&src.written);
    }
}

#[derive(Clone, Copy)]
pub struct Checkpoint {
    accounts: u32,
    slots: u32,
}

pub struct BlockState {
    accounts: UndoTable<Account>,
    slots: UndoTable<U256>,
}

impl Default for BlockState {
    fn default() -> BlockState {
        BlockState { accounts: UndoTable::new(MAX_ACCOUNTS), slots: UndoTable::new(MAX_SLOTS) }
    }
}

impl BlockState {
    pub fn account(&self, keys: &KeyTable, id: u32) -> Account {
        self.accounts.get(id).unwrap_or_else(|| keys.parent_account(id).1)
    }

    pub fn slot(&self, keys: &KeyTable, id: u32) -> U256 {
        self.slots.get(id).unwrap_or_else(|| keys.parent_slot(id).2)
    }

    pub fn set_account(&mut self, id: u32, value: Account) -> bool {
        self.accounts.set(id, value)
    }

    pub fn set_slot(&mut self, id: u32, value: U256) -> bool {
        self.slots.set(id, value)
    }

    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            accounts: self.accounts.undo_log.len() as u32,
            slots: self.slots.undo_log.len() as u32,
        }
    }

    pub fn undo_to(&mut self, at: Checkpoint) {
        self.accounts.undo_to(at.accounts);
        self.slots.undo_to(at.slots);
    }

    pub fn clear_undo(&mut self) {
        self.accounts.undo_log.clear();
        self.slots.undo_log.clear();
    }

    pub fn reset(&mut self) {
        self.accounts.reset();
        self.slots.reset();
    }

    pub fn accounts_since(&self, at: Checkpoint) -> impl Iterator<Item = u32> + '_ {
        self.accounts.undo_log[at.accounts as usize..].iter().map(|u| u.0)
    }

    pub fn slots_since(&self, at: Checkpoint) -> impl Iterator<Item = u32> + '_ {
        self.slots.undo_log[at.slots as usize..].iter().map(|u| u.0)
    }

    pub fn absorb_run(
        &mut self,
        keys: &mut KeyTable,
        db: &GeneralizedDatabase,
    ) -> Result<(), &'static str> {
        for (&address, account) in &db.current_accounts_state {
            if account.is_unmodified() {
                continue;
            }
            let initial = db.initial_accounts_state.get(&address).ok_or("state_fold")?;
            let destroyed = matches!(
                account.status,
                AccountStatus::Destroyed | AccountStatus::DestroyedModified
            );
            // Wiping parent storage needs all of it; post-Cancun only same-tx creations are
            // destroyed.
            if destroyed && initial.has_storage {
                return Err("destroyed_storage");
            }
            let now = Account::from_levm(account);
            if now != Account::from_levm(initial) {
                if now.code_hash != initial.info.code_hash &&
                    !keys.deployed_codes.contains_key(&now.code_hash)
                {
                    let code = db.codes.get(&now.code_hash).ok_or("state_fold")?;
                    keys.deployed_codes.insert(now.code_hash, code.clone());
                }
                let id = keys.intern_account(address)?;
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
                let id = keys.intern_slot(address, key)?;
                if !self.slots.set(id, value) {
                    return Err("state_full");
                }
            }
        }
        Ok(())
    }

    pub fn accounts_diff<'a>(
        &'a self,
        held: &'a BlockState,
        keys: &'a KeyTable,
    ) -> impl Iterator<Item = (u32, Account)> + 'a {
        self.accounts.diff(&held.accounts, move |id| keys.parent_account(id).1)
    }

    pub fn slots_diff<'a>(
        &'a self,
        held: &'a BlockState,
        keys: &'a KeyTable,
    ) -> impl Iterator<Item = (u32, U256)> + 'a {
        self.slots.diff(&held.slots, move |id| keys.parent_slot(id).2)
    }

    pub fn copy_from(&mut self, src: &BlockState) {
        self.accounts.copy_from(&src.accounts);
        self.slots.copy_from(&src.slots);
    }
}

struct EvmView {
    keys: Cell<*const KeyTable>,
    state: Cell<*const BlockState>,
}

// SAFETY: levm calls in only from the engine thread, inside `EngineVm::with_state`.
unsafe impl Send for EvmView {}
unsafe impl Sync for EvmView {}

impl EvmView {
    fn current(&self) -> (&KeyTable, &BlockState) {
        let (keys, state) = (self.keys.get(), self.state.get());
        assert!(!keys.is_null() && !state.is_null(), "merge view used outside a run");
        // SAFETY: `with_state` sets both from references outliving the run, and nulls them after.
        unsafe { (&*keys, &*state) }
    }
}

impl Database for EvmView {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        let (keys, state) = self.current();
        match keys.account_ids.get(&address) {
            Some(&id) => Ok(state.account(keys, id).to_state()),
            None => keys.db.get_account_state(address),
        }
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        let (keys, state) = self.current();
        match keys.slot_ids.get(&(address, key)) {
            Some(&id) => Ok(state.slot(keys, id)),
            None => keys.db.get_storage_value(address, key),
        }
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        self.current().0.db.get_block_hash(block_number)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.current().0.db.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        let keys = self.current().0;
        match keys.deployed_codes.get(&code_hash) {
            Some(code) => Ok(code.clone()),
            None => keys.db.get_account_code(code_hash),
        }
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        let keys = self.current().0;
        match keys.deployed_codes.get(&code_hash) {
            Some(code) => Ok(CodeMetadata { length: code.len() as u64 }),
            None => keys.db.get_code_metadata(code_hash),
        }
    }

    fn precompile_cache(&self) -> Option<&PrecompileCache> {
        self.current().0.db.precompile_cache()
    }
}

pub struct EngineVm {
    pub evm: Evm,
    view: Arc<EvmView>,
}

impl Default for EngineVm {
    fn default() -> EngineVm {
        let view =
            Arc::new(EvmView { keys: Cell::new(ptr::null()), state: Cell::new(ptr::null()) });
        let evm = Evm {
            db: GeneralizedDatabase::new(view.clone()),
            vm_type: VMType::L1,
            crypto: Arc::new(NativeCrypto),
            stateless_validator: None,
        };
        EngineVm { evm, view }
    }
}

impl EngineVm {
    pub fn with_state<R>(
        &mut self,
        keys: &KeyTable,
        state: &BlockState,
        f: impl FnOnce(&mut Evm) -> R,
    ) -> R {
        struct UnbindView<'a>(&'a EvmView);
        impl Drop for UnbindView<'_> {
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
        let _clear = UnbindView(&self.view);
        f(&mut self.evm)
    }
}
