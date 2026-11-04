use ethrex_common::{Address, H256, types::AccountState, utils::keccak};
use ethrex_crypto::keccak::keccak_hash;
use ethrex_levm::db::Database;
use ethrex_rlp::encode::RLPEncode;
use ethrex_storage::Store;
use ethrex_trie::TrieDB;
use rustc_hash::FxHashMap;

use crate::{
    merge::{
        state::{Account, BlockState, KeyTable},
        trie::{NodeArena, TrieHasher, Tries},
    },
    metrics,
};

const OWN_SLOTS: usize = 1 << 16;
const OWN_HASHES: usize = 1 << 14;

pub struct StateTries {
    parent_root: H256,
    // None while closed: nothing below is valid for the current parent.
    state_db: Option<Box<dyn TrieDB>>,
    own_nodes: NodeArena,
    hasher: TrieHasher,
    state_trie: u32,
    storage_tries: FxHashMap<Address, (u32, Box<dyn TrieDB>)>,
    emitted: BlockState,
    leaves: Vec<(Address, AccountState, bool)>,
    leaf_ix: FxHashMap<Address, u32>,
    storage_ids: Vec<u32>,
    trie_roots: Vec<H256>,
    rlp_buf: Vec<u8>,
}

impl Default for StateTries {
    fn default() -> StateTries {
        StateTries {
            parent_root: H256::zero(),
            state_db: None,
            own_nodes: NodeArena::new(true, OWN_SLOTS, OWN_HASHES),
            hasher: TrieHasher::default(),
            state_trie: 0,
            storage_tries: FxHashMap::default(),
            emitted: BlockState::default(),
            leaves: Vec::with_capacity(1024),
            leaf_ix: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            storage_ids: Vec::with_capacity(1024),
            trie_roots: Vec::with_capacity(1024),
            rlp_buf: Vec::with_capacity(128),
        }
    }
}

impl StateTries {
    pub fn is_open(&self) -> bool {
        self.state_db.is_some()
    }

    pub fn close(&mut self) {
        self.state_db = None;
    }

    // Own nodes link into `nodes`, so reopen whenever `nodes` may have been cleared.
    pub fn open(
        &mut self,
        store: &Store,
        parent: H256,
        nodes: &mut NodeArena,
    ) -> Result<(), &'static str> {
        self.state_db = Some(store.state_trie_db(parent).map_err(|_| "state_trie")?);
        self.parent_root = parent;
        self.own_nodes.clear();
        self.state_trie = Tries { parent: nodes, own: &mut self.own_nodes }.open_root(parent);
        self.storage_tries.clear();
        self.emitted.reset();
        Ok(())
    }

    pub fn update(
        &mut self,
        store: &Store,
        parent: &dyn Database,
        nodes: &mut NodeArena,
        keys: &KeyTable,
        now: &BlockState,
    ) -> Result<H256, &'static str> {
        if self.own_nodes.node_count() > OWN_SLOTS / 2 {
            metrics::rejection("state_root", "own_arena_reset");
            self.open(store, self.parent_root, nodes)?;
        }
        let spilled = self.own_nodes.spilled;
        self.leaves.clear();
        self.leaf_ix.clear();
        for (id, acct) in now.accounts_diff(&self.emitted, keys) {
            let (address, _) = keys.parent_account(id);
            queue_leaf(
                &mut self.leaves,
                &mut self.leaf_ix,
                parent,
                keys,
                address,
                Some((id, acct)),
            )?;
        }
        for (id, value) in now.slots_diff(&self.emitted, keys) {
            let (address, key, _) = keys.parent_slot(id);
            let acct = keys.find_account(address).map(|id| (id, now.account(keys, id)));
            let i = queue_leaf(&mut self.leaves, &mut self.leaf_ix, parent, keys, address, acct)?;
            let mut tries = Tries { parent: nodes, own: &mut self.own_nodes };
            let (trie, db) = match self.storage_tries.entry(address) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let db = store
                        .storage_trie_db(keccak(address), self.parent_root)
                        .map_err(|_| "storage_trie")?;
                    e.insert((tries.open_root(self.leaves[i].1.storage_root), db))
                }
            };
            self.rlp_buf.clear();
            value.encode(&mut self.rlp_buf);
            let value = (!value.is_zero()).then_some(&self.rlp_buf[..]);
            *trie = tries.put(db.as_ref(), *trie, &keccak_hash(key.0), value)?;
        }
        let mut tries = Tries { parent: nodes, own: &mut self.own_nodes };
        self.storage_ids.clear();
        self.storage_ids.extend(
            self.leaves.iter().filter_map(|(a, ..)| self.storage_tries.get(a).map(|s| s.0)),
        );
        tries.commit(&mut self.hasher, &self.storage_ids, &mut self.trie_roots);
        let mut roots = self.trie_roots.iter();
        let state_db = self.state_db.as_deref().ok_or("state_trie_closed")?;
        for (address, leaf, removed) in &mut self.leaves {
            let storage = self.storage_tries.contains_key(address);
            if storage {
                leaf.storage_root = *roots.next().ok_or("state_root")?;
            }
            let path = keccak(*address);
            if *removed || *leaf == AccountState::default() {
                if *removed && storage {
                    return Err("state_root_removed_storage");
                }
                self.state_trie = tries.put(state_db, self.state_trie, &path.0, None)?;
                continue;
            }
            self.rlp_buf.clear();
            leaf.encode(&mut self.rlp_buf);
            self.state_trie = tries.put(state_db, self.state_trie, &path.0, Some(&self.rlp_buf))?;
        }
        tries.commit(&mut self.hasher, &[self.state_trie], &mut self.trie_roots);
        if self.own_nodes.spilled != spilled {
            metrics::rejection("state_root", "parent_arena_full");
        }
        self.emitted.copy_from(now);
        Ok(self.trie_roots[0])
    }
}

fn queue_leaf(
    leaves: &mut Vec<(Address, AccountState, bool)>,
    queued: &mut FxHashMap<Address, u32>,
    parent: &dyn Database,
    keys: &KeyTable,
    address: Address,
    acct: Option<(u32, Account)>,
) -> Result<usize, &'static str> {
    if let Some(&i) = queued.get(&address) {
        return Ok(i as usize);
    }
    let (leaf, removed) = match acct {
        Some((id, now)) => {
            let leaf = AccountState {
                nonce: now.nonce,
                balance: now.balance,
                storage_root: keys.parent_storage_root(id),
                code_hash: now.code_hash,
            };
            (leaf, now.is_empty() && !keys.parent_account(id).1.is_empty())
        }
        // Touched only through storage, so never interned as an account.
        None => (parent.get_account_state(address).map_err(|_| "state_root")?, false),
    };
    queued.insert(address, leaves.len() as u32);
    leaves.push((address, leaf, removed));
    Ok(leaves.len() - 1)
}
