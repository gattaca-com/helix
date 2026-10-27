//! State and storage tries for state roots. Nodes are fixed-size values in arenas and a child is a
//! `u32` id. The parent's nodes go in one arena per slot, each decoded once on first use and shared
//! by every builder's tries; a builder's writes copy the nodes on their path into its own arena and
//! change them in place there. `commit` encodes and hashes each changed node once, a depth at a
//! time across every trie it is given, through `keccak::many`.
//!
//! Child ids: 0 is empty. `OWN` marks an id in the builder's arena, else the parent's; `HASH` marks
//! an index into that arena's `hashes` (a node known by hash, loaded from the store on first use),
//! else into its `slots`. Index 0 of both is a sentinel.
//!
//! ```ignore
//! let mut tries = Tries { parent: &mut parent_arena, own: &mut builder_arena };
//! let root = tries.root(state_root);
//! let root = tries.put(db, root, &keccak(address), Some(&account_rlp))?;
//! tries.commit(&mut hasher, &[root], &mut hashes);
//! ```
//!
//! A load into a full parent arena goes to the builder's arena, unshared. A full builder arena is
//! `Err(FULL)`: the caller drops its tries and starts again from the parent.
//!
//! The parent arena outlives the slot: a node's hash fixes its contents, so nodes loaded under one
//! parent serve the next by hash. `trim` between slots clears it once over half full.

use ethrex_common::{H256, constants::EMPTY_TRIE_HASH};
use ethrex_crypto::keccak::keccak_hash;
use ethrex_trie::{Nibbles, TrieDB};
use rustc_hash::FxHashMap;

use crate::merge::{
    keccak,
    rlp::{Ref, bytes_len, compact_len, put_bytes, put_compact, put_list_header, put_ref, ref_len},
};

pub const FULL: &str = "trie_full";
const MISSING: &str = "trie_missing_node";
const DECODE: &str = "trie_decode";
const STORE: &str = "trie_store";

const OWN: u32 = 1 << 31;
const HASH: u32 = 1 << 30;
const INDEX: u32 = HASH - 1;
/// Account RLP is at most about 110 bytes, a storage value 33.
const VALUE_MAX: usize = 128;
/// Nibbles in a hashed key.
const KEY: usize = 64;
/// Slots and hashes one decode can add: a branch's 16 children, each possibly an inline node.
const DECODE_MAX: usize = 33;

#[derive(Clone, Copy)]
struct Path {
    len: u8,
    nib: [u8; KEY],
}

impl Path {
    fn of(nibbles: &[u8]) -> Path {
        Path::join(&[], nibbles)
    }

    fn join(a: &[u8], b: &[u8]) -> Path {
        debug_assert!(a.len() + b.len() <= KEY);
        let mut path = Path { len: (a.len() + b.len()) as u8, nib: [0; KEY] };
        path.nib[..a.len()].copy_from_slice(a);
        path.nib[a.len()..a.len() + b.len()].copy_from_slice(b);
        path
    }

    fn get(&self) -> &[u8] {
        &self.nib[..self.len as usize]
    }
}

#[derive(Clone, Copy)]
struct Value {
    len: u8,
    data: [u8; VALUE_MAX],
}

impl Value {
    fn of(bytes: &[u8]) -> Result<Value, &'static str> {
        if bytes.len() > VALUE_MAX {
            return Err(DECODE);
        }
        let mut value = Value { len: bytes.len() as u8, data: [0; VALUE_MAX] };
        value.data[..bytes.len()].copy_from_slice(bytes);
        Ok(value)
    }

    fn get(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

// Every slot is branch-sized either way; nodes live by value.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum Node {
    Leaf { path: Path, value: Value },
    Ext { path: Path, child: u32 },
    Branch { children: [u32; 16] },
}

#[derive(Clone, Copy)]
struct Slot {
    node: Node,
    /// The node's reference in its parent; empty when it changed since the last commit.
    cached: Ref,
}

pub struct Arena {
    tag: u32,
    slots: Vec<Slot>,
    /// Nodes known by hash, and each one's slot id once loaded.
    hashes: Vec<(H256, u32)>,
    /// Trie roots by hash, so every builder opening a trie gets the same id.
    roots: FxHashMap<H256, u32>,
    /// Slot id of each node loaded by hash; parent arena only.
    known: FxHashMap<H256, u32>,
    /// Parent nodes decoded into this builder's arena because the parent's was full.
    pub spilled: u32,
}

impl Arena {
    /// `own`: a builder's arena rather than the parent's.
    pub fn new(own: bool, slots: usize, hashes: usize) -> Arena {
        let mut arena = Arena {
            tag: if own { OWN } else { 0 },
            slots: Vec::with_capacity(slots),
            hashes: Vec::with_capacity(hashes),
            roots: FxHashMap::default(),
            known: FxHashMap::with_capacity_and_hasher(
                if own { 0 } else { slots },
                Default::default(),
            ),
            spilled: 0,
        };
        arena.clear();
        arena
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.hashes.clear();
        self.roots.clear();
        self.known.clear();
        self.spilled = 0;
        let empty = Node::Branch { children: [0; 16] };
        self.slots.push(Slot { node: empty, cached: Ref::default() });
        self.hashes.push((H256::zero(), 0));
    }

    pub fn used(&self) -> usize {
        self.slots.len()
    }

    /// Keeps the arena for the next parent unless it is over half full.
    pub fn trim(&mut self) {
        if 2 * self.slots.len() > self.slots.capacity() ||
            2 * self.hashes.len() > self.hashes.capacity()
        {
            self.clear();
        }
    }

    fn room(&self) -> bool {
        self.slots.len() + DECODE_MAX <= self.slots.capacity() &&
            self.hashes.len() + DECODE_MAX <= self.hashes.capacity()
    }
}

/// Encoding and hashing buffers for `commit`, kept across calls.
#[derive(Default)]
pub struct Hasher {
    stack: Vec<(u32, usize)>,
    levels: Vec<Vec<u32>>,
    enc: Vec<u8>,
    long: Vec<(u32, u32, u32)>,
    spans: Vec<(u32, u32)>,
    hashes: Vec<[u8; 32]>,
}

pub struct Tries<'a> {
    pub parent: &'a mut Arena,
    pub own: &'a mut Arena,
}

impl Tries<'_> {
    fn arena(&self, id: u32) -> &Arena {
        if id & OWN != 0 { self.own } else { self.parent }
    }

    fn arena_mut(&mut self, tag: u32) -> &mut Arena {
        if tag & OWN != 0 { self.own } else { self.parent }
    }

    fn slot(&self, id: u32) -> &Slot {
        let slots = &self.arena(id).slots;
        debug_assert!(id & HASH == 0 && ((id & INDEX) as usize) < slots.len());
        &slots[(id & INDEX) as usize]
    }

    /// The id of the parent trie with root `hash`.
    pub fn root(&mut self, hash: H256) -> u32 {
        if hash == *EMPTY_TRIE_HASH {
            return 0;
        }
        for arena in [&*self.parent, &*self.own] {
            if let Some(&id) = arena.roots.get(&hash) {
                return id;
            }
        }
        let arena = match self.parent.room() {
            true => &mut *self.parent,
            false => {
                self.own.spilled += 1;
                &mut *self.own
            }
        };
        let id = arena.tag | HASH | arena.hashes.len() as u32;
        arena.hashes.push((hash, 0));
        arena.roots.insert(hash, id);
        id
    }

    /// Sets `key` to `value`, or removes it, in the trie under `root`; returns the new root.
    pub fn put(
        &mut self,
        db: &dyn TrieDB,
        root: u32,
        key: &[u8; 32],
        value: Option<&[u8]>,
    ) -> Result<u32, &'static str> {
        let value = value.map(Value::of).transpose()?;
        Ok(self.put_at(db, root, &nibbles(key), 0, value.as_ref())?.0)
    }

    /// The subtree's new id, and whether anything under it changed. A changed node's ancestors
    /// must all be rewritten, as `commit` stops at nodes with a cached reference.
    fn put_at(
        &mut self,
        db: &dyn TrieDB,
        id: u32,
        key: &[u8; KEY],
        depth: usize,
        value: Option<&Value>,
    ) -> Result<(u32, bool), &'static str> {
        let id = self.load(db, id, &key[..depth])?;
        let rest = &key[depth..];
        if id == 0 {
            return match value {
                Some(&value) => Ok((self.alloc(Node::Leaf { path: Path::of(rest), value })?, true)),
                None => Ok((0, false)),
            };
        }
        match self.slot(id).node {
            Node::Leaf { path, value: old } => {
                if path.get() == rest {
                    return match value {
                        Some(&value) => Ok((self.write(id, Node::Leaf { path, value })?, true)),
                        None => Ok((0, true)),
                    };
                }
                let Some(&value) = value else { return Ok((id, false)) };
                // Keys are all `KEY` nibbles, so the two paths differ before either ends.
                let c = common(path.get(), rest);
                let old = self
                    .write(id, Node::Leaf { path: Path::of(&path.get()[c + 1..]), value: old })?;
                let new = self.alloc(Node::Leaf { path: Path::of(&rest[c + 1..]), value })?;
                let mut children = [0; 16];
                children[path.nib[c] as usize] = old;
                children[rest[c] as usize] = new;
                Ok((self.branch_under(&rest[..c], children)?, true))
            }
            Node::Ext { path, child } => {
                let c = common(path.get(), rest);
                if c == path.len as usize {
                    let (new, changed) = self.put_at(db, child, key, depth + c, value)?;
                    if !changed {
                        return Ok((id, false));
                    }
                    return Ok((self.extend(id, path.get(), new)?, true));
                }
                let Some(&value) = value else { return Ok((id, false)) };
                let tail = match path.len as usize - c - 1 {
                    0 => child,
                    _ => {
                        self.write(id, Node::Ext { path: Path::of(&path.get()[c + 1..]), child })?
                    }
                };
                let new = self.alloc(Node::Leaf { path: Path::of(&rest[c + 1..]), value })?;
                let mut children = [0; 16];
                children[path.nib[c] as usize] = tail;
                children[rest[c] as usize] = new;
                Ok((self.branch_under(&rest[..c], children)?, true))
            }
            Node::Branch { mut children } => {
                let n = rest[0] as usize;
                let (new, changed) = self.put_at(db, children[n], key, depth + 1, value)?;
                if !changed {
                    return Ok((id, false));
                }
                children[n] = new;
                let mut left = children.iter().enumerate().filter(|(_, c)| **c != 0);
                let (only, more) = (left.next(), left.next());
                let id = match (only, more) {
                    (Some((i, &child)), None) => {
                        // One child left: the branch folds into it.
                        let mut prefix = [0; KEY];
                        prefix[..depth].copy_from_slice(&key[..depth]);
                        prefix[depth] = i as u8;
                        let child = self.load(db, child, &prefix[..depth + 1])?;
                        self.extend(id, &[i as u8], child)?
                    }
                    (None, _) => 0,
                    _ => self.write(id, Node::Branch { children })?,
                };
                Ok((id, true))
            }
        }
    }

    /// Node `child` under the nibbles `prefix`, written over `id`: a leaf or extension child
    /// absorbs the prefix, a branch gets an extension.
    fn extend(&mut self, id: u32, prefix: &[u8], child: u32) -> Result<u32, &'static str> {
        if child == 0 {
            return Ok(0);
        }
        let node = match self.slot(child).node {
            Node::Leaf { path, value } => {
                Node::Leaf { path: Path::join(prefix, path.get()), value }
            }
            Node::Ext { path, child } => Node::Ext { path: Path::join(prefix, path.get()), child },
            Node::Branch { .. } => Node::Ext { path: Path::of(prefix), child },
        };
        self.write(id, node)
    }

    fn branch_under(&mut self, prefix: &[u8], children: [u32; 16]) -> Result<u32, &'static str> {
        let branch = self.alloc(Node::Branch { children })?;
        match prefix.is_empty() {
            true => Ok(branch),
            false => self.alloc(Node::Ext { path: Path::of(prefix), child: branch }),
        }
    }

    /// `node` in place of `id`: in place when `id` is the builder's, else in a new slot.
    fn write(&mut self, id: u32, node: Node) -> Result<u32, &'static str> {
        if id & OWN == 0 {
            return self.alloc(node);
        }
        self.own.slots[(id & INDEX) as usize] = Slot { node, cached: Ref::default() };
        Ok(id)
    }

    fn alloc(&mut self, node: Node) -> Result<u32, &'static str> {
        let slots = &mut self.own.slots;
        if slots.len() == slots.capacity() {
            return Err(FULL);
        }
        slots.push(Slot { node, cached: Ref::default() });
        Ok(OWN | (slots.len() - 1) as u32)
    }

    /// The slot id behind `id`, at nibble path `prefix` from its trie's root; loads it from `db`
    /// if it is known only by hash.
    fn load(&mut self, db: &dyn TrieDB, id: u32, prefix: &[u8]) -> Result<u32, &'static str> {
        if id & HASH == 0 {
            return Ok(id);
        }
        let ix = (id & INDEX) as usize;
        let (hash, loaded) = self.arena(id).hashes[ix];
        if loaded != 0 {
            return Ok(loaded);
        }
        if let Some(&node) = self.parent.known.get(&hash) {
            self.arena_mut(id).hashes[ix].1 = node;
            return Ok(node);
        }
        let encoded = db
            .get(Nibbles::from_hex(prefix.to_vec()))
            .map_err(|_| STORE)?
            .filter(|e| !e.is_empty())
            .ok_or(MISSING)?;
        let to = if id & OWN == 0 && self.parent.room() { 0 } else { OWN };
        if to & OWN != 0 && !self.own.room() {
            return Err(FULL);
        }
        let node = self.decode(to, &encoded, Ref { len: 32, data: hash.0 })?;
        // The parent arena is shared, so it never points into a builder's.
        match to == id & OWN {
            true => self.arena_mut(id).hashes[ix].1 = node,
            false => self.own.spilled += 1,
        }
        if to == 0 {
            self.parent.known.insert(hash, node);
        }
        Ok(node)
    }

    /// Decodes node `encoded` into the arena tagged `to`; `cached` is its reference.
    fn decode(&mut self, to: u32, encoded: &[u8], cached: Ref) -> Result<u32, &'static str> {
        let (list, mut payload, _) = item(encoded)?;
        if !list {
            return Err(DECODE);
        }
        let mut items = [(false, &[][..], &[][..]); 17];
        let mut n = 0;
        while !payload.is_empty() {
            let (list, inner, len) = item(payload)?;
            *items.get_mut(n).ok_or(DECODE)? = (list, inner, &payload[..len]);
            payload = &payload[len..];
            n += 1;
        }
        let node = match n {
            17 => {
                // The tries hold values only at full-length keys, never on a branch.
                if items[16].0 || !items[16].1.is_empty() {
                    return Err(DECODE);
                }
                let mut children = [0; 16];
                for (child, item) in children.iter_mut().zip(&items) {
                    *child = self.child(to, *item)?;
                }
                Node::Branch { children }
            }
            2 => {
                let (list, compact, _) = items[0];
                let (&first, rest) = compact.split_first().ok_or(DECODE)?;
                if list || first >> 4 > 3 {
                    return Err(DECODE);
                }
                let mut path = Path { len: 0, nib: [0; KEY] };
                let mut push = |nibble: u8| {
                    *path.nib.get_mut(path.len as usize).ok_or(DECODE)? = nibble;
                    path.len += 1;
                    Ok::<_, &'static str>(())
                };
                if first & 0x10 != 0 {
                    push(first & 0x0f)?;
                }
                for &b in rest {
                    push(b >> 4)?;
                    push(b & 0x0f)?;
                }
                match first & 0x20 != 0 {
                    true if !items[1].0 => Node::Leaf { path, value: Value::of(items[1].1)? },
                    true => return Err(DECODE),
                    false => Node::Ext { path, child: self.child(to, items[1])? },
                }
            }
            _ => return Err(DECODE),
        };
        let arena = self.arena_mut(to);
        arena.slots.push(Slot { node, cached });
        Ok(to | (arena.slots.len() - 1) as u32)
    }

    /// A child reference item: empty, a hash, or a node inlined under 32 bytes.
    fn child(&mut self, to: u32, item: (bool, &[u8], &[u8])) -> Result<u32, &'static str> {
        match item {
            (true, _, whole) => {
                let mut r = Ref { len: whole.len() as u8, data: [0; 32] };
                r.data.get_mut(..whole.len()).ok_or(DECODE)?.copy_from_slice(whole);
                self.decode(to, whole, r)
            }
            (false, [], _) => Ok(0),
            (false, hash, _) if hash.len() == 32 => {
                let arena = self.arena_mut(to);
                arena.hashes.push((H256::from_slice(hash), 0));
                Ok(to | HASH | (arena.hashes.len() - 1) as u32)
            }
            _ => Err(DECODE),
        }
    }

    fn reference(&self, id: u32) -> Option<Ref> {
        match id {
            0 => None,
            _ if id & HASH != 0 => {
                let (hash, _) = self.arena(id).hashes[(id & INDEX) as usize];
                Some(Ref { len: 32, data: hash.0 })
            }
            _ => Some(self.slot(id).cached),
        }
    }

    /// Hashes every node changed under `roots` and writes each root's hash to `out`.
    pub fn commit(&mut self, h: &mut Hasher, roots: &[u32], out: &mut Vec<H256>) {
        for level in &mut h.levels {
            level.clear();
        }
        for &root in roots {
            h.stack.push((root, 0));
        }
        while let Some((id, depth)) = h.stack.pop() {
            if id & OWN == 0 || id & HASH != 0 || self.slot(id).cached.len != 0 {
                continue;
            }
            if h.levels.len() <= depth {
                h.levels.resize_with(depth + 1, Vec::new);
            }
            h.levels[depth].push(id);
            match self.slot(id).node {
                Node::Leaf { .. } => {}
                Node::Ext { child, .. } => h.stack.push((child, depth + 1)),
                Node::Branch { children } => {
                    h.stack.extend(children.iter().map(|&c| (c, depth + 1)));
                }
            }
        }
        for depth in (0..h.levels.len()).rev() {
            h.enc.clear();
            h.long.clear();
            for i in 0..h.levels[depth].len() {
                let id = h.levels[depth][i];
                let at = h.enc.len();
                self.encode(id, &mut h.enc);
                let len = h.enc.len() - at;
                if len < 32 {
                    let cached = &mut self.own.slots[(id & INDEX) as usize].cached;
                    cached.len = len as u8;
                    cached.data[..len].copy_from_slice(&h.enc[at..]);
                    h.enc.truncate(at);
                } else {
                    h.long.push((id, at as u32, len as u32));
                }
            }
            h.long.sort_unstable_by_key(|&(_, _, len)| len);
            h.spans.clear();
            h.spans.extend(h.long.iter().map(|&(_, at, len)| (at, len)));
            h.hashes.resize(h.spans.len(), [0; 32]);
            keccak::many(&h.enc, &h.spans, &mut h.hashes);
            for (&(id, ..), hash) in h.long.iter().zip(&h.hashes) {
                self.own.slots[(id & INDEX) as usize].cached = Ref { len: 32, data: *hash };
            }
        }
        out.clear();
        out.extend(roots.iter().map(|&root| match self.reference(root) {
            None => *EMPTY_TRIE_HASH,
            Some(r) if r.len == 32 => H256(r.data),
            Some(r) => H256(keccak_hash(r.get())),
        }));
    }

    /// Appends node `id`'s encoding; its children are already referenced.
    fn encode(&self, id: u32, enc: &mut Vec<u8>) {
        match self.slot(id).node {
            Node::Leaf { path, value } => {
                put_list_header(enc, compact_len(path.get()) + bytes_len(value.get()));
                put_compact(enc, path.get(), true);
                put_bytes(enc, value.get());
            }
            Node::Ext { path, child } => {
                let child = self.reference(child).unwrap_or_default();
                put_list_header(enc, compact_len(path.get()) + ref_len(&child));
                put_compact(enc, path.get(), false);
                put_ref(enc, &child);
            }
            Node::Branch { children } => {
                let len: usize =
                    children.iter().map(|&c| self.reference(c).map_or(1, |r| ref_len(&r))).sum();
                put_list_header(enc, len + 1);
                for &c in &children {
                    match self.reference(c) {
                        Some(r) => put_ref(enc, &r),
                        None => enc.push(0x80),
                    }
                }
                enc.push(0x80);
            }
        }
    }
}

fn nibbles(key: &[u8; 32]) -> [u8; KEY] {
    std::array::from_fn(|i| if i % 2 == 0 { key[i / 2] >> 4 } else { key[i / 2] & 0x0f })
}

fn common(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The RLP item at the start of `data`: whether it is a list, its payload, and its whole length.
fn item(data: &[u8]) -> Result<(bool, &[u8], usize), &'static str> {
    let (&b, rest) = data.split_first().ok_or(DECODE)?;
    let long = |n: u8| -> Result<(usize, usize), &'static str> {
        let n = n as usize;
        let len = rest.get(..n).ok_or(DECODE)?.iter().fold(0usize, |l, &b| (l << 8) | b as usize);
        Ok((1 + n, len))
    };
    let (list, (at, len)) = match b {
        0x00..=0x7f => return Ok((false, &data[..1], 1)),
        0x80..=0xb7 => (false, (1, (b - 0x80) as usize)),
        0xb8..=0xbf => (false, long(b - 0xb7)?),
        0xc0..=0xf7 => (true, (1, (b - 0xc0) as usize)),
        _ => (true, long(b - 0xf7)?),
    };
    Ok((list, data.get(at..at + len).ok_or(DECODE)?, at + len))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ethrex_crypto::{keccak::keccak_hash, native::NativeCrypto};
    use ethrex_trie::{InMemoryTrieDB, Trie, db::NodeMap};

    use super::{Arena, Hasher, Tries};

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Random writes, rewrites and removals in rounds, on a parent committed to a store; the
    /// root must match ethrex's trie after every round. Each parent's final state is committed to
    /// a fresh store as the next parent, which reuses the trimmed parent arena.
    fn check(key_of: impl Fn(u64) -> [u8; 32], keys: u64, value_max: u64, rng: &mut Rng) {
        let value = |rng: &mut Rng| -> Vec<u8> {
            (0..1 + rng.next() % value_max).map(|_| rng.next() as u8).collect()
        };
        let mut model = BTreeMap::new();
        for k in 0..keys / 2 {
            model.insert(key_of(k), value(rng));
        }
        let (mut parent, mut own) =
            (Arena::new(false, 1 << 15, 1 << 18), Arena::new(true, 1 << 14, 1 << 16));
        let (mut hasher, mut out) = (Hasher::default(), Vec::new());
        for n in 0..3 {
            let map = NodeMap::default();
            let mut want = Trie::new(Box::new(InMemoryTrieDB::new(map.clone())));
            for (k, v) in &model {
                want.insert(k.to_vec(), v.clone()).unwrap();
            }
            want.commit(&NativeCrypto).unwrap();
            let db = InMemoryTrieDB::new(map);
            parent.trim();
            own.clear();
            let mut tries = Tries { parent: &mut parent, own: &mut own };
            let mut root = tries.root(want.hash_no_commit(&NativeCrypto));
            for round in 0..8 {
                for _ in 0..200 {
                    let key = key_of(rng.next() % keys);
                    // Later rounds mostly remove, to fold branches back.
                    let v = (rng.next() % 8 >= round).then(|| value(rng));
                    match &v {
                        Some(v) => {
                            want.insert(key.to_vec(), v.clone()).unwrap();
                            model.insert(key, v.clone());
                        }
                        None => {
                            want.remove(&key).unwrap();
                            model.remove(&key);
                        }
                    }
                    root = tries.put(&db, root, &key, v.as_deref()).unwrap();
                }
                tries.commit(&mut hasher, &[root], &mut out);
                assert_eq!(out[0], want.hash_no_commit(&NativeCrypto), "parent {n} round {round}");
            }
        }
        assert!(!parent.known.is_empty(), "trim kept the parent arena");
    }

    #[test]
    fn matches_the_trie_root() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        // Hashed keys: long leaf paths, hashed nodes.
        check(|k| keccak_hash(k.to_be_bytes()), 2000, 110, &mut rng);
        // Keys sharing all but their last byte: short leaves inlined in their branch.
        check(|k| std::array::from_fn(|i| if i == 31 { k as u8 } else { 7 }), 256, 3, &mut rng);
    }
}
