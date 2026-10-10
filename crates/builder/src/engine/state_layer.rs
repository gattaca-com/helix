use std::{
    sync::{Arc, mpsc::Sender},
    thread::JoinHandle,
};

use ethrex_common::{
    H256,
    types::{AccountInfo, AccountState, AccountUpdate},
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::{db::gen_db::GeneralizedDatabase, errors::VMError};
use ethrex_rlp::{decode::RLPDecode, encode::RLPEncode};
use ethrex_storage::{Store, error::StoreError, hash_address, hash_key};
use ethrex_trie::{EMPTY_TRIE_HASH, Nibbles, NodeRef, Trie, TrieDB, TrieError};
use flux_profiler::timed;
use rustc_hash::{FxHashMap, FxHashSet};

/// The parent state with the base block applied, held in memory so each
/// emission hashes only what changed after the base.
pub struct StateLayer {
    store: Store,
    parent_state_root: H256,
    state: Trie,
    /// Storage tries the base changed; their roots exist only here.
    storage: FxHashMap<H256, Trie>,
    /// Set when a parallel apply failed after taking storage tries out of
    /// `storage`; those changes cannot be undone, so no root is trusted.
    broken: bool,
    /// Parent value of every key a base of this slot changed, so the next base from the same
    /// builder can be layered on this one and restore what it no longer changes.
    originals: FxHashMap<TrieKey, Option<Vec<u8>>>,
    /// Keys the current base wrote.
    touched: FxHashSet<TrieKey>,
    /// Cleared by an account removal or storage wipe, which `originals` cannot undo.
    reusable: bool,
    /// Leave storage roots stale while a base streams in and hash each touched trie once at
    /// the end: hot contracts change in nearly every batch.
    defer_storage_roots: bool,
    /// Accounts whose storage root is stale under `defer_storage_roots`.
    storage_dirty: FxHashSet<H256>,
}

/// A state-trie key (`None`) or a storage key of the account with that hashed address.
type TrieKey = (Option<H256>, Vec<u8>);
type Undo = Vec<(Option<H256>, Vec<u8>, Option<Vec<u8>>)>;

/// Kept off the global pool so layer builds never queue behind order presims.
static LAYER_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .thread_name(|i| format!("layer-{i}"))
        .build()
        .expect("layer pool")
});

/// Emission's root updates run here, so they never queue a layer build.
static EMIT_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .thread_name(|i| format!("emit-root-{i}"))
        .build()
        .expect("emit pool")
});

pub type UpdateChunk = Arc<Vec<AccountUpdate>>;

/// Replay's end of the layer build: drains committed updates to the builder thread.
pub struct LayerFeed {
    tx: Sender<UpdateChunk>,
    /// Every chunk sent so far, so a replay checkpoint can re-seed a later build.
    pub streamed: Vec<UpdateChunk>,
}

impl LayerFeed {
    #[timed]
    pub fn flush(&mut self, db: &mut GeneralizedDatabase) -> Result<(), VMError> {
        let chunk = Arc::new(db.get_state_transitions_tx()?);
        if !chunk.is_empty() {
            let _unused = self.tx.send(chunk.clone());
            self.streamed.push(chunk);
        }
        Ok(())
    }
}

/// What the layer thread does once the base is built: applies each committed order's state
/// changes as they arrive, and answers emissions with the root of the layer plus their own.
enum LayerMsg {
    Orders(UpdateChunk),
    Root(Vec<AccountUpdate>, Sender<Result<H256, StoreError>>),
}

pub struct PendingLayer {
    msgs: Option<Sender<LayerMsg>>,
    build: Option<JoinHandle<Result<StateLayer, StoreError>>>,
}

impl PendingLayer {
    /// Hands the state changes of an order just committed to the layer thread.
    pub fn send_orders(&self, chunk: UpdateChunk) {
        if let Some(msgs) = &self.msgs {
            let _unused = msgs.send(LayerMsg::Orders(chunk));
        }
    }

    /// State root of the base, every order sent so far, and `updates`.
    #[timed]
    pub fn root(&self, updates: Vec<AccountUpdate>) -> Result<H256, StoreError> {
        let wait = std::time::Instant::now();
        let (reply, answer) = std::sync::mpsc::channel();
        self.msgs
            .as_ref()
            .and_then(|msgs| msgs.send(LayerMsg::Root(updates, reply)).ok())
            .ok_or_else(|| StoreError::Custom("layer thread gone".into()))?;
        let root = answer.recv().map_err(|_| StoreError::Custom("layer build failed".into()))?;
        crate::metrics::stage_latency("layer_wait", wait.elapsed().as_micros() as u64);
        root
    }

    /// The layer, base and orders included, for the builder's next base to start from.
    pub fn take(&mut self) -> Option<StateLayer> {
        self.msgs = None;
        let layer = self.build.take()?.join().ok()?.ok()?;
        (layer.reusable && !layer.broken).then_some(layer)
    }
}

impl StateLayer {
    /// Builds the layer on its own thread from `streamed` and whatever the
    /// returned feed sends until it is dropped.
    /// Builds the layer on its own thread from `streamed` and whatever the
    /// returned feed sends until it is dropped, on top of `prev` when it holds the same parent.
    pub fn spawn(
        store: &Store,
        parent_state_root: H256,
        streamed: Vec<UpdateChunk>,
        prev: Option<StateLayer>,
        verify: bool,
    ) -> (LayerFeed, PendingLayer) {
        let (tx, rx) = std::sync::mpsc::channel::<UpdateChunk>();
        let (msgs, inbox) = std::sync::mpsc::channel::<LayerMsg>();
        for chunk in &streamed {
            let _unused = tx.send(chunk.clone());
        }
        let store = store.clone();
        let warming = crate::metrics::warming();
        let build = std::thread::spawn(move || {
            crate::metrics::set_warming(warming);
            let prev = prev.filter(|layer| layer.parent_state_root == parent_state_root);
            let mut reused = prev.is_some();
            let mut layer = match prev {
                Some(layer) => layer,
                None => Self::fresh(store.clone(), parent_state_root)?,
            };
            let mut seen = Vec::new();
            while let Ok(chunk) = rx.recv() {
                let mut batch = vec![chunk];
                batch.extend(rx.try_iter());
                seen.extend(batch.iter().cloned());
                let apply = std::time::Instant::now();
                layer.apply_batch(&batch, true)?;
                if reused && !layer.reusable {
                    reused = false;
                    layer = Self::fresh(store.clone(), parent_state_root)?;
                    layer.apply_batch(&seen, true)?;
                }
                crate::metrics::stage_latency("layer_apply", apply.elapsed().as_micros() as u64);
            }
            if let Err(err) = layer.restore_untouched() {
                if !reused {
                    return Err(err);
                }
                reused = false;
                layer = Self::fresh(store.clone(), parent_state_root)?;
                layer.apply_batch(&seen, true)?;
                layer.restore_untouched()?;
            }
            let hash = std::time::Instant::now();
            let root = layer.state.hash_no_commit(&NativeCrypto);
            crate::metrics::stage_latency("layer_hash", hash.elapsed().as_micros() as u64);
            crate::metrics::layer_build(if reused { "reused" } else { "fresh" });
            if reused && verify {
                let mut check = Self::fresh(store.clone(), parent_state_root)?;
                check.defer_storage_roots = false;
                check.apply_batch(&seen, true)?;
                if check.state.hash_no_commit(&NativeCrypto) != root {
                    crate::metrics::layer_build("verify_mismatch");
                    tracing::error!(%root, "reused state layer root differs from a fresh build");
                    check.restore_untouched()?;
                    layer = check;
                } else {
                    crate::metrics::layer_build("verified");
                }
            }
            let mut orders: Vec<UpdateChunk> = Vec::new();
            let mut pending: Vec<UpdateChunk> = Vec::new();
            while let Ok(msg) = inbox.recv() {
                let mut next = Some(msg);
                while let Some(msg) = next.take() {
                    match msg {
                        LayerMsg::Orders(chunk) => {
                            pending.push(chunk);
                            next = inbox.try_recv().ok();
                        }
                        LayerMsg::Root(updates, reply) => {
                            let root = layer.order_root(&mut pending, &mut orders, &updates);
                            let root = match (root, verify) {
                                (Ok(root), true) => {
                                    let full = Self::fresh(store.clone(), parent_state_root)
                                        .and_then(|mut check| {
                                            check.defer_storage_roots = false;
                                            check.apply_batch(&seen, true)?;
                                            check.restore_untouched()?;
                                            check.apply_batch(&orders, false)?;
                                            check.root_with(&updates)
                                        });
                                    crate::metrics::layer_build(
                                        if full.as_ref().ok() == Some(&root) {
                                            "emit_verified"
                                        } else {
                                            "emit_mismatch"
                                        },
                                    );
                                    full
                                }
                                (root, _) => root,
                            };
                            let _unused = reply.send(root);
                        }
                    }
                }
                if !pending.is_empty() {
                    let batch = std::mem::take(&mut pending);
                    layer.apply_batch(&batch, false)?;
                    orders.extend(batch);
                }
            }
            if !pending.is_empty() {
                layer.apply_batch(&pending, false)?;
            }
            Ok(layer)
        });
        (LayerFeed { tx, streamed }, PendingLayer { msgs: Some(msgs), build: Some(build) })
    }

    /// A layer on the parent that hashes storage as it applies, for B to patch directly.
    pub(crate) fn open(store: Store, parent_state_root: H256) -> Result<Self, StoreError> {
        let mut layer = Self::fresh(store, parent_state_root)?;
        layer.defer_storage_roots = false;
        Ok(layer)
    }

    pub(crate) fn apply_root(&mut self, updates: &[AccountUpdate]) -> Result<H256, StoreError> {
        self.apply(updates, None)?;
        Ok(self.state.hash_no_commit(&NativeCrypto))
    }

    fn fresh(store: Store, parent_state_root: H256) -> Result<Self, StoreError> {
        Ok(Self {
            state: open_state(&store, parent_state_root)?,
            store,
            parent_state_root,
            storage: FxHashMap::default(),
            broken: false,
            originals: FxHashMap::default(),
            touched: FxHashSet::default(),
            reusable: true,
            defer_storage_roots: true,
            storage_dirty: FxHashSet::default(),
        })
    }

    /// Puts every key an earlier base changed but this one did not back to its parent value,
    /// then fixes the storage root of any account this base wrote whose storage that moved.
    fn restore_untouched(&mut self) -> Result<(), StoreError> {
        let stale: Vec<(TrieKey, Option<Vec<u8>>)> = self
            .originals
            .iter()
            .filter(|(key, _)| !self.touched.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut restored_storage = FxHashSet::default();
        for ((account, key), original) in stale {
            let trie = match account {
                Some(account) => {
                    restored_storage.insert(account);
                    self.storage
                        .get_mut(&account)
                        .ok_or_else(|| StoreError::Custom("restored slot has no trie".into()))?
                }
                None => &mut self.state,
            };
            if trie.get(&key)? == original {
                continue;
            }
            match original {
                Some(value) => trie.insert(key, value)?,
                None => {
                    trie.remove(&key)?;
                }
            }
        }
        let restored_written: Vec<H256> = restored_storage
            .into_iter()
            .filter(|account| self.touched.contains(&(None, account.as_bytes().to_vec())))
            .collect();
        self.storage_dirty.extend(restored_written);
        self.touched.clear();
        self.finish_storage_roots()
    }

    /// Hashes every stale storage trie once, in parallel, and writes the roots into their
    /// account leaves.
    fn finish_storage_roots(&mut self) -> Result<(), StoreError> {
        let dirty: Vec<H256> = self.storage_dirty.drain().collect();
        let tries: Vec<(H256, Trie)> = dirty
            .into_iter()
            .filter_map(|hashed| Some((hashed, self.storage.remove(&hashed)?)))
            .collect();
        let hashed_tries: Vec<(H256, H256, Trie)> = LAYER_POOL.install(|| {
            use rayon::prelude::*;
            tries
                .into_par_iter()
                .map(|(hashed, trie)| (hashed, trie.hash_no_commit(&NativeCrypto), trie))
                .collect()
        });
        for (hashed, storage_root, trie) in hashed_tries {
            self.storage.insert(hashed, trie);
            let Some(encoded) = self.state.get(hashed.as_bytes())? else { continue };
            let mut state = AccountState::decode(&encoded)?;
            if state.storage_root != storage_root {
                state.storage_root = storage_root;
                self.state.insert(hashed.as_bytes().to_vec(), state.encode_to_vec())?;
            }
        }
        Ok(())
    }

    /// Applies queued order changes, then returns the root with `updates` on top.
    fn order_root(
        &mut self,
        pending: &mut Vec<UpdateChunk>,
        orders: &mut Vec<UpdateChunk>,
        updates: &[AccountUpdate],
    ) -> Result<H256, StoreError> {
        if !pending.is_empty() {
            let batch = std::mem::take(pending);
            self.apply_batch(&batch, false)?;
            orders.extend(batch);
        }
        self.finish_storage_roots()?;
        self.root_with(updates)
    }

    /// State root of the layer plus `updates`; the layer is left as it was.
    pub fn root_with(&mut self, updates: &[AccountUpdate]) -> Result<H256, StoreError> {
        if self.broken {
            return Err(StoreError::Custom("state layer broken by a failed apply".into()));
        }
        if updates.iter().any(|update| update.removed_storage) {
            return Err(StoreError::Custom("storage removal after the base".into()));
        }
        // Tries share nodes copy-on-write, so restoring these roots leaves the layer exactly
        // as it was.
        let mut storage_roots: Vec<(H256, NodeRef)> = Vec::new();
        for update in updates.iter().filter(|update| !update.added_storage.is_empty()) {
            let hashed = H256::from_slice(&hash_address(&update.address));
            if !self.storage.contains_key(&hashed) {
                let storage_root = match self.state.get(hashed.as_bytes())? {
                    Some(encoded) => AccountState::decode(&encoded)?.storage_root,
                    None => EMPTY_TRIE_HASH,
                };
                let trie = open_storage(&self.store, hashed, self.parent_state_root, storage_root)?;
                self.storage.insert(hashed, trie);
            }
            if let Some(trie) = self.storage.get(&hashed) {
                storage_roots.push((hashed, trie.root.clone()));
            }
        }
        let state_root = self.state.root.clone();
        let root = if updates.iter().any(|update| update.removed) {
            self.apply(updates, None).map(|()| self.state.hash_no_commit(&NativeCrypto))
        } else {
            let applied = self.apply_parallel(updates, None, &EMIT_POOL, false);
            self.broken = applied.is_err();
            applied.map(|()| self.state.hash_no_commit(&NativeCrypto))
        };
        self.state.root = state_root;
        for (hashed, root) in storage_roots {
            if let Some(trie) = self.storage.get_mut(&hashed) {
                trie.root = root;
            }
        }
        root
    }

    /// Applies every queued chunk at once: updates merge per account and each
    /// account's storage trie is rebuilt on its own rayon task. Removals are
    /// order-sensitive, so a batch holding one takes the serial path.
    /// `base` chunks are this base's own changes; order chunks change it further and are
    /// restored like anything else the next base leaves untouched.
    fn apply_batch(&mut self, chunks: &[UpdateChunk], base: bool) -> Result<(), StoreError> {
        let updates = || chunks.iter().flat_map(|chunk| chunk.iter());
        if updates().any(|update| update.removed || update.removed_storage) {
            self.reusable = false;
            for chunk in chunks {
                self.apply(chunk, None)?;
            }
            return Ok(());
        }
        let mut merged: FxHashMap<ethrex_common::Address, AccountUpdate> = FxHashMap::default();
        for update in updates() {
            let entry = merged
                .entry(update.address)
                .or_insert_with(|| AccountUpdate { address: update.address, ..Default::default() });
            if update.info.is_some() {
                entry.info = update.info.clone();
            }
            entry.added_storage.extend(update.added_storage.iter().map(|(k, v)| (*k, *v)));
        }
        let mut merged: Vec<AccountUpdate> = merged.into_values().collect();
        // An update without `info` keeps the fields this base started from, which on a layer
        // built on an earlier base are the parent's, not the leaf's.
        for update in merged.iter_mut().filter(|update| base && update.info.is_none()) {
            let key = (None, hash_address(&update.address).to_vec());
            if self.touched.contains(&key) {
                continue;
            }
            if let Some(Some(original)) = self.originals.get(&key) {
                let state = AccountState::decode(original)?;
                update.info = Some(AccountInfo {
                    nonce: state.nonce,
                    balance: state.balance,
                    code_hash: state.code_hash,
                });
            }
        }
        let mut undo = Vec::new();
        self.apply_parallel(&merged, Some(&mut undo), &LAYER_POOL, self.defer_storage_roots)?;
        for (account, key, old) in undo {
            let key = (account, key);
            if base {
                self.touched.insert(key.clone());
            }
            self.originals.entry(key).or_insert(old);
        }
        Ok(())
    }

    /// Applies `updates`, one per account and none removed, with each account's
    /// storage trie rebuilt on its own layer-pool task.
    fn apply_parallel(
        &mut self,
        updates: &[AccountUpdate],
        mut undo: Option<&mut Undo>,
        pool: &rayon::ThreadPool,
        defer: bool,
    ) -> Result<(), StoreError> {
        let record = undo.is_some();
        let mut accounts = Vec::with_capacity(updates.len());
        for update in updates {
            let hashed = H256::from_slice(&hash_address(&update.address));
            let old = self.state.get(hashed.as_bytes())?;
            let account = match &old {
                Some(encoded) => AccountState::decode(encoded)?,
                None => AccountState::default(),
            };
            let storage = (!update.added_storage.is_empty()).then(|| self.storage.remove(&hashed));
            accounts.push((hashed, old, account, update, storage));
        }
        let (store, parent_state_root) = (&self.store, self.parent_state_root);
        type Rebuilt = (
            H256,
            Option<Vec<u8>>,
            AccountState,
            Option<Trie>,
            Vec<(Vec<u8>, Option<Vec<u8>>)>,
            bool,
        );
        let rebuilt: Vec<Rebuilt> = pool.install(|| {
            use rayon::prelude::*;
            accounts
                .into_par_iter()
                .map(|(hashed, old, mut account, update, storage)| {
                    if let Some(info) = &update.info {
                        account.nonce = info.nonce;
                        account.balance = info.balance;
                        account.code_hash = info.code_hash;
                    }
                    let mut slot_undo = Vec::new();
                    let Some(existing) = storage else {
                        return Ok((hashed, old, account, None, slot_undo, false));
                    };
                    let mut trie = match existing {
                        Some(trie) => trie,
                        None => {
                            open_storage(&store, hashed, parent_state_root, account.storage_root)?
                        }
                    };
                    let mut changed = !record;
                    for (slot, value) in &update.added_storage {
                        let key = hash_key(slot);
                        let new = (!value.is_zero()).then(|| value.encode_to_vec());
                        if record {
                            let old = trie.get(&key)?;
                            // An unchanged slot keeps its path's cached hashes.
                            if old == new {
                                slot_undo.push((key, old));
                                continue;
                            }
                            slot_undo.push((key.clone(), old));
                        }
                        changed = true;
                        match new {
                            Some(new) => trie.insert(key, new)?,
                            None => {
                                trie.remove(&key)?;
                            }
                        }
                    }
                    if !changed {
                        return Ok((hashed, old, account, Some(trie), slot_undo, false));
                    }
                    if !defer {
                        account.storage_root = trie.hash_no_commit(&NativeCrypto);
                    }
                    Ok((hashed, old, account, Some(trie), slot_undo, true))
                })
                .collect::<Result<_, StoreError>>()
        })?;
        for (hashed, old, account, trie, slot_undo, storage_changed) in rebuilt {
            let encoded = account.encode_to_vec();
            let unchanged = record && old.as_deref() == Some(encoded.as_slice());
            if let Some(undo) = undo.as_deref_mut() {
                undo.push((None, hashed.as_bytes().to_vec(), old));
                undo.extend(slot_undo.into_iter().map(|(key, old)| (Some(hashed), key, old)));
            }
            if let Some(trie) = trie {
                self.storage.insert(hashed, trie);
                if defer && storage_changed {
                    self.storage_dirty.insert(hashed);
                }
            }
            if !unchanged {
                self.state.insert(hashed.as_bytes().to_vec(), encoded)?;
            }
        }
        Ok(())
    }

    fn apply(
        &mut self,
        updates: &[AccountUpdate],
        mut undo: Option<&mut Undo>,
    ) -> Result<(), StoreError> {
        for update in updates {
            let hashed = H256::from_slice(&hash_address(&update.address));
            let old = self.state.get(hashed.as_bytes())?;
            if let Some(undo) = undo.as_deref_mut() {
                undo.push((None, hashed.as_bytes().to_vec(), old.clone()));
            }
            if update.removed {
                self.state.remove(hashed.as_bytes())?;
                continue;
            }
            let mut account = match &old {
                Some(encoded) => AccountState::decode(encoded)?,
                None => AccountState::default(),
            };
            if update.removed_storage {
                account.storage_root = EMPTY_TRIE_HASH;
                self.storage.remove(&hashed);
            }
            if let Some(info) = &update.info {
                account.nonce = info.nonce;
                account.balance = info.balance;
                account.code_hash = info.code_hash;
            }
            if !update.added_storage.is_empty() {
                let storage = match self.storage.entry(hashed) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => entry.insert(open_storage(
                        &self.store,
                        hashed,
                        self.parent_state_root,
                        account.storage_root,
                    )?),
                };
                for (slot, value) in &update.added_storage {
                    let key = hash_key(slot);
                    if let Some(undo) = undo.as_deref_mut() {
                        undo.push((Some(hashed), key.clone(), storage.get(&key)?));
                    }
                    if value.is_zero() {
                        storage.remove(&key)?;
                    } else {
                        storage.insert(key, value.encode_to_vec())?;
                    }
                }
                account.storage_root = storage.hash_no_commit(&NativeCrypto);
            }
            self.state.insert(hashed.as_bytes().to_vec(), account.encode_to_vec())?;
        }
        Ok(())
    }
}

/// Parent trie nodes keyed by account (`None` for the state trie) and path.
type Nodes = Arc<dashmap::DashMap<(Option<H256>, Vec<u8>), Option<Vec<u8>>>>;

/// Every layer in a slot opens its tries at the same parent root, so a parent node read once
/// serves every builder's layer instead of going back to the database each time.
fn nodes(parent_state_root: H256) -> Nodes {
    static CACHE: std::sync::Mutex<Option<(H256, Nodes)>> = std::sync::Mutex::new(None);
    let Ok(mut cache) = CACHE.lock() else { return Nodes::default() };
    match cache.as_ref() {
        Some((root, nodes)) if *root == parent_state_root => nodes.clone(),
        _ => {
            let nodes = Nodes::default();
            *cache = Some((parent_state_root, nodes.clone()));
            nodes
        }
    }
}

struct CachedNodes {
    inner: Box<dyn TrieDB>,
    account: Option<H256>,
    nodes: Nodes,
}

impl TrieDB for CachedNodes {
    fn get(&self, key: Nibbles) -> Result<Option<Vec<u8>>, TrieError> {
        let cache_key = (self.account, key.as_ref().to_vec());
        if let Some(node) = self.nodes.get(&cache_key) {
            return Ok(node.clone());
        }
        let node = self.inner.get(key)?;
        self.nodes.insert(cache_key, node.clone());
        Ok(node)
    }

    fn put_batch(&self, key_values: Vec<(Nibbles, Vec<u8>)>) -> Result<(), TrieError> {
        self.inner.put_batch(key_values)
    }

    fn flatkeyvalue_computed(&self, key: Nibbles) -> bool {
        self.inner.flatkeyvalue_computed(key)
    }
}

fn open_state(store: &Store, parent_state_root: H256) -> Result<Trie, StoreError> {
    let nodes = nodes(parent_state_root);
    store.open_state_trie_with(parent_state_root, |inner| {
        Box::new(CachedNodes { inner, account: None, nodes })
    })
}

fn open_storage(
    store: &Store,
    hashed: H256,
    parent_state_root: H256,
    storage_root: H256,
) -> Result<Trie, StoreError> {
    let nodes = nodes(parent_state_root);
    store.open_storage_trie_with(hashed, parent_state_root, storage_root, |inner| {
        Box::new(CachedNodes { inner, account: Some(hashed), nodes })
    })
}

/// The parent's state-trie node store, read through the slot's node cache.
pub(crate) fn cached_state_db(
    store: &Store,
    parent_state_root: H256,
) -> Result<Box<dyn TrieDB>, StoreError> {
    Ok(Box::new(CachedNodes {
        inner: store.state_trie_db(parent_state_root)?,
        account: None,
        nodes: nodes(parent_state_root),
    }))
}

/// An account's storage-trie node store in the parent, read through the slot's node cache.
pub(crate) fn cached_storage_db(
    store: &Store,
    hashed: H256,
    parent_state_root: H256,
) -> Result<Box<dyn TrieDB>, StoreError> {
    Ok(Box::new(CachedNodes {
        inner: store.storage_trie_db(hashed, parent_state_root)?,
        account: Some(hashed),
        nodes: nodes(parent_state_root),
    }))
}
