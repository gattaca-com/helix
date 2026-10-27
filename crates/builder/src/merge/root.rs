//! One builder's state root across its merges of a slot. Its tries hold the parent state plus the
//! state of the last block this builder emitted (`held`, by `Keys` id); an emission writes only
//! the ids whose value differs from `held`'s, so only their paths are loaded and re-hashed. An
//! emptied account is removed; storage is never wiped (`State::fold` refuses that). Parent nodes
//! are decoded once per slot into the parent's `Arena`, shared by every builder's root.
//!
//! The builder's arena keeps the copies replaced by later emissions. Past half full it is cleared
//! and the state is written in full against the parent; a single block fits well within half.

use ethrex_common::{Address, H256, types::AccountState, utils::keccak};
use ethrex_crypto::keccak::keccak_hash;
use ethrex_levm::db::Database;
use ethrex_rlp::encode::RLPEncode;
use ethrex_storage::Store;
use ethrex_trie::TrieDB;
use rustc_hash::FxHashMap;

use crate::{
    merge::{
        state::{Acct, Keys, State},
        trie::{Arena, Hasher, Tries},
    },
    metrics,
};

/// Builder arena slots: a full update writes up to ~2.5k keys, each copying its path (~10 nodes).
const OWN_SLOTS: usize = 1 << 16;
const OWN_HASHES: usize = 1 << 14;

pub struct Root {
    parent: H256,
    state_db: Box<dyn TrieDB>,
    own: Arena,
    hasher: Hasher,
    state: u32,
    /// Opened at an account's first storage write: its trie, holding its parent storage plus
    /// `held`'s slots, and its store reader.
    storage: FxHashMap<Address, (u32, Box<dyn TrieDB>)>,
    /// What the tries hold beyond the parent.
    held: State,
    /// This update's account leaves, each once, and whether each is removed.
    leaves: Vec<(Address, AccountState, bool)>,
    /// Index in `leaves` by address.
    queued: FxHashMap<Address, u32>,
    ids: Vec<u32>,
    roots: Vec<H256>,
    value: Vec<u8>,
}

impl Root {
    pub fn open(store: &Store, parent: H256, nodes: &mut Arena) -> Result<Root, &'static str> {
        let mut own = Arena::new(true, OWN_SLOTS, OWN_HASHES);
        let state = Tries { parent: nodes, own: &mut own }.root(parent);
        Ok(Root {
            parent,
            state_db: store.state_trie_db(parent).map_err(|_| "state_trie")?,
            own,
            hasher: Hasher::default(),
            state,
            storage: FxHashMap::default(),
            held: State::default(),
            leaves: Vec::with_capacity(1024),
            queued: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            ids: Vec::with_capacity(1024),
            roots: Vec::with_capacity(1024),
            value: Vec::with_capacity(128),
        })
    }

    /// The root of the parent state with `now` written over it. On `Err` the tries are
    /// part-written: drop the `Root`.
    pub fn update(
        &mut self,
        store: &Store,
        parent: &dyn Database,
        nodes: &mut Arena,
        keys: &Keys,
        now: &State,
    ) -> Result<H256, &'static str> {
        if self.own.used() > OWN_SLOTS / 2 {
            metrics::rejection("state_root", "own_arena_reset");
            self.own.clear();
            self.state = Tries { parent: nodes, own: &mut self.own }.root(self.parent);
            self.storage.clear();
            self.held.reset();
        }
        let spilled = self.own.spilled;
        self.leaves.clear();
        self.queued.clear();
        for (id, acct) in now.accounts_unlike(&self.held, keys) {
            let (address, before) = keys.account_at(id);
            queue(&mut self.leaves, &mut self.queued, parent, address, Some((acct, before)))?;
        }
        for (id, value) in now.slots_unlike(&self.held, keys) {
            let (address, key, _) = keys.slot_at(id);
            let acct =
                keys.account_id(address).map(|id| (now.account(keys, id), keys.account_at(id).1));
            let i = queue(&mut self.leaves, &mut self.queued, parent, address, acct)?;
            let mut tries = Tries { parent: nodes, own: &mut self.own };
            let (trie, db) = match self.storage.entry(address) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let db = store
                        .storage_trie_db(keccak(address), self.parent)
                        .map_err(|_| "storage_trie")?;
                    e.insert((tries.root(self.leaves[i].1.storage_root), db))
                }
            };
            self.value.clear();
            value.encode(&mut self.value);
            let value = (!value.is_zero()).then_some(&self.value[..]);
            *trie = tries.put(db.as_ref(), *trie, &keccak_hash(key.0), value)?;
        }
        let mut tries = Tries { parent: nodes, own: &mut self.own };
        self.ids.clear();
        self.ids.extend(self.leaves.iter().filter_map(|(a, ..)| self.storage.get(a).map(|s| s.0)));
        tries.commit(&mut self.hasher, &self.ids, &mut self.roots);
        let mut roots = self.roots.iter();
        for (address, leaf, removed) in &mut self.leaves {
            let storage = self.storage.contains_key(address);
            if storage {
                leaf.storage_root = *roots.next().ok_or("state_root")?;
            }
            let path = keccak(*address);
            // Post-merge the trie holds no empty account.
            if *removed || *leaf == AccountState::default() {
                // Dropping the storage trie would leave `held`'s slots of it unwritten.
                if *removed && storage {
                    return Err("state_root_removed_storage");
                }
                self.state = tries.put(self.state_db.as_ref(), self.state, &path.0, None)?;
                continue;
            }
            self.value.clear();
            leaf.encode(&mut self.value);
            self.state =
                tries.put(self.state_db.as_ref(), self.state, &path.0, Some(&self.value))?;
        }
        tries.commit(&mut self.hasher, &[self.state], &mut self.roots);
        if self.own.spilled != spilled {
            metrics::rejection("state_root", "parent_arena_full");
        }
        self.held.copy_from(now);
        Ok(self.roots[0])
    }
}

/// Index in `leaves` of `address`'s leaf, queued at the first call: its parent leaf with the
/// fields of `acct` (its value now and in the parent), when it has one.
fn queue(
    leaves: &mut Vec<(Address, AccountState, bool)>,
    queued: &mut FxHashMap<Address, u32>,
    parent: &dyn Database,
    address: Address,
    acct: Option<(Acct, Acct)>,
) -> Result<usize, &'static str> {
    if let Some(&i) = queued.get(&address) {
        return Ok(i as usize);
    }
    let mut leaf = parent.get_account_state(address).map_err(|_| "state_root")?;
    let mut removed = false;
    if let Some((now, before)) = acct {
        leaf.nonce = now.nonce;
        leaf.balance = now.balance;
        leaf.code_hash = now.code_hash;
        removed = now.is_empty() && !before.is_empty();
    }
    queued.insert(address, leaves.len() as u32);
    leaves.push((address, leaf, removed));
    Ok(leaves.len() - 1)
}
