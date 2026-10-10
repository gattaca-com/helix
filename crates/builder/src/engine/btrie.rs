//! B's Merkle Patricia trie. Nodes are fixed-size values in one arena (no per-node heap), the
//! parent's nodes are decoded once per slot into a cache every builder's trie copies from, writes
//! mutate in place, and only nodes a write touched are re-encoded and hashed.

use std::sync::Arc;

use ethrex_common::{
    H256, U256,
    constants::EMPTY_TRIE_HASH,
    types::{AccountState, AccountUpdate},
};
use ethrex_crypto::keccak::keccak_hash;
use ethrex_rlp::{decode::RLPDecode, encode::RLPEncode};
use ethrex_trie::{Nibbles, TrieDB, TrieError};
use rustc_hash::FxHashMap;

type Id = u32;

/// A nibble path of at most 64 nibbles.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Path {
    len: u8,
    nib: [u8; 64],
}

impl Path {
    const EMPTY: Path = Path { len: 0, nib: [0; 64] };

    fn of(nibbles: &[u8]) -> Self {
        let mut path = Path::EMPTY;
        path.nib[..nibbles.len()].copy_from_slice(nibbles);
        path.len = nibbles.len() as u8;
        path
    }

    fn get(&self) -> &[u8] {
        &self.nib[..self.len as usize]
    }

    fn push(&mut self, nibble: u8) {
        self.nib[self.len as usize] = nibble;
        self.len += 1;
    }

    fn extend(&mut self, nibbles: &[u8]) {
        let at = self.len as usize;
        self.nib[at..at + nibbles.len()].copy_from_slice(nibbles);
        self.len += nibbles.len() as u8;
    }
}

/// A node's reference in its parent: a hash, or its whole encoding when under 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ref {
    Hash(H256),
    Inline { len: u8, data: [u8; 31] },
}

#[derive(Clone, Copy)]
enum Child {
    Empty,
    Unloaded(Ref),
    Node(Id),
}

/// A leaf value: an account's RLP or a storage value's.
#[derive(Clone, Copy)]
struct Value {
    len: u8,
    data: [u8; 128],
}

impl Value {
    fn of(bytes: &[u8]) -> Result<Self, TrieError> {
        if bytes.len() > 128 {
            return Err(TrieError::Verify("leaf value over 128 bytes".into()));
        }
        let mut value = Value { len: bytes.len() as u8, data: [0; 128] };
        value.data[..bytes.len()].copy_from_slice(bytes);
        Ok(value)
    }

    fn get(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

#[derive(Clone, Copy)]
enum Node {
    Leaf { path: Path, value: Value },
    Ext { path: Path, child: Child },
    Branch { children: [Child; 16] },
}

struct Slot {
    node: Node,
    /// The node's reference in its parent; `None` once a write below it changed it.
    cached: Option<Ref>,
}

/// The parent's nodes, decoded once per slot, by account (`None` for the account trie) and path.
type Shared = Arc<dashmap::DashMap<(Option<H256>, Path), Node, rustc_hash::FxBuildHasher>>;

fn shared(parent_root: H256) -> Shared {
    static CACHE: std::sync::Mutex<Option<(H256, Shared)>> = std::sync::Mutex::new(None);
    let Ok(mut cache) = CACHE.lock() else { return Shared::default() };
    match cache.as_ref() {
        Some((root, shared)) if *root == parent_root => shared.clone(),
        _ => {
            let shared = Shared::default();
            *cache = Some((parent_root, shared.clone()));
            shared
        }
    }
}

pub struct Mpt {
    slots: Vec<Slot>,
    root: Child,
    db: Box<dyn TrieDB>,
    account: Option<H256>,
    shared: Shared,
}

fn nibbles(key: &[u8]) -> Path {
    let mut path = Path { len: (key.len() * 2) as u8, nib: [0; 64] };
    for (i, b) in key.iter().enumerate() {
        path.nib[2 * i] = b >> 4;
        path.nib[2 * i + 1] = b & 0x0f;
    }
    path
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl Mpt {
    fn open(db: Box<dyn TrieDB>, root: H256, account: Option<H256>, shared: Shared) -> Self {
        let root =
            if root == *EMPTY_TRIE_HASH { Child::Empty } else { Child::Unloaded(Ref::Hash(root)) };
        Mpt { slots: Vec::with_capacity(256), root, db, account, shared }
    }

    fn push(&mut self, node: Node, cached: Option<Ref>) -> Id {
        self.slots.push(Slot { node, cached });
        (self.slots.len() - 1) as Id
    }

    /// The arena id behind `child` at `path` from the root, loading it if needed.
    fn load(&mut self, child: Child, path: &Path) -> Result<Option<Id>, TrieError> {
        let r = match child {
            Child::Empty => return Ok(None),
            Child::Node(id) => return Ok(Some(id)),
            Child::Unloaded(r) => r,
        };
        let node = match &r {
            Ref::Inline { len, data } => decode(&data[..*len as usize])?,
            Ref::Hash(_) => {
                let key = (self.account, *path);
                match self.shared.get(&key) {
                    Some(node) => *node,
                    None => {
                        let encoded = self
                            .db
                            .get(Nibbles::from_hex(path.get().to_vec()))?
                            .filter(|bytes| !bytes.is_empty())
                            .ok_or_else(|| TrieError::Verify("node missing".into()))?;
                        let node = decode(&encoded)?;
                        self.shared.insert(key, node);
                        node
                    }
                }
            }
        };
        Ok(Some(self.push(node, Some(r))))
    }

    fn get(&mut self, key: &[u8]) -> Result<Option<Value>, TrieError> {
        let rest = nibbles(key);
        let rest = rest.get();
        let mut path = Path::EMPTY;
        let mut child = self.root;
        let mut at = 0;
        loop {
            let Some(id) = self.load(child, &path)? else { return Ok(None) };
            match &self.slots[id as usize].node {
                Node::Leaf { path: p, value } => {
                    return Ok((p.get() == &rest[at..]).then_some(*value));
                }
                Node::Ext { path: p, child: c } => {
                    if !rest[at..].starts_with(p.get()) {
                        return Ok(None);
                    }
                    path.extend(p.get());
                    at += p.len as usize;
                    child = *c;
                }
                Node::Branch { children } => {
                    let Some(&ix) = rest.get(at) else { return Ok(None) };
                    child = children[ix as usize];
                    path.push(ix);
                    at += 1;
                }
            }
        }
    }

    /// Sets `key` to `value`, or removes it when `value` is `None`.
    fn set(&mut self, key: &[u8], value: Option<Value>) -> Result<(), TrieError> {
        let rest = nibbles(key);
        let mut path = Path::EMPTY;
        self.root = match value {
            Some(value) => self.insert(self.root, &mut path, rest.get(), value)?,
            None => self.remove(self.root, &mut path, rest.get())?,
        };
        Ok(())
    }

    fn leaf(&mut self, path: &[u8], value: Value) -> Child {
        Child::Node(self.push(Node::Leaf { path: Path::of(path), value }, None))
    }

    fn insert(
        &mut self,
        child: Child,
        path: &mut Path,
        rest: &[u8],
        value: Value,
    ) -> Result<Child, TrieError> {
        let Some(id) = self.load(child, path)? else { return Ok(self.leaf(rest, value)) };
        self.slots[id as usize].cached = None;
        let replaced = match self.slots[id as usize].node {
            Node::Leaf { path: p, .. } if p.get() == rest => Node::Leaf { path: p, value },
            Node::Leaf { path: p, value: old } => {
                let p = p.get();
                let common = common_prefix(p, rest);
                let mut children = [Child::Empty; 16];
                children[p[common] as usize] = self.leaf(&p[common + 1..], old);
                children[rest[common] as usize] = self.leaf(&rest[common + 1..], value);
                self.wrap(&rest[..common], Node::Branch { children })
            }
            Node::Ext { path: p, child: c } if rest.starts_with(p.get()) => {
                let depth = path.len;
                path.extend(p.get());
                let c = self.insert(c, path, &rest[p.len as usize..], value)?;
                path.len = depth;
                Node::Ext { path: p, child: c }
            }
            Node::Ext { path: p, child: c } => {
                let p = p.get();
                let common = common_prefix(p, rest);
                let mut children = [Child::Empty; 16];
                children[p[common] as usize] = if p.len() > common + 1 {
                    Child::Node(
                        self.push(Node::Ext { path: Path::of(&p[common + 1..]), child: c }, None),
                    )
                } else {
                    c
                };
                children[rest[common] as usize] = self.leaf(&rest[common + 1..], value);
                self.wrap(&rest[..common], Node::Branch { children })
            }
            Node::Branch { mut children } => {
                let ix = rest[0] as usize;
                path.push(rest[0]);
                children[ix] = self.insert(children[ix], path, &rest[1..], value)?;
                path.len -= 1;
                Node::Branch { children }
            }
        };
        self.slots[id as usize].node = replaced;
        Ok(Child::Node(id))
    }

    /// `node` behind an extension of `prefix`, when there is one.
    fn wrap(&mut self, prefix: &[u8], node: Node) -> Node {
        if prefix.is_empty() {
            return node;
        }
        Node::Ext { path: Path::of(prefix), child: Child::Node(self.push(node, None)) }
    }

    fn remove(&mut self, child: Child, path: &mut Path, rest: &[u8]) -> Result<Child, TrieError> {
        let Some(id) = self.load(child, path)? else { return Ok(Child::Empty) };
        let replaced = match self.slots[id as usize].node {
            Node::Leaf { path: p, .. } if p.get() == rest => return Ok(Child::Empty),
            Node::Ext { path: p, child: c } if rest.starts_with(p.get()) => {
                let depth = path.len;
                path.extend(p.get());
                let c = self.remove(c, path, &rest[p.len as usize..])?;
                let merged = self.merge(p.get(), c, path)?;
                path.len = depth;
                match merged {
                    Some(node) => node,
                    None => return Ok(Child::Empty),
                }
            }
            Node::Branch { mut children } => {
                let ix = rest[0] as usize;
                path.push(rest[0]);
                children[ix] = self.remove(children[ix], path, &rest[1..])?;
                path.len -= 1;
                let mut live =
                    children.iter().enumerate().filter(|(_, c)| !matches!(c, Child::Empty));
                match (live.next(), live.next()) {
                    (None, _) => return Ok(Child::Empty),
                    (Some((only, &only_child)), None) => {
                        path.push(only as u8);
                        let merged = self.merge(&[only as u8], only_child, path)?;
                        path.len -= 1;
                        match merged {
                            Some(node) => node,
                            None => return Ok(Child::Empty),
                        }
                    }
                    _ => Node::Branch { children },
                }
            }
            // The key is not in this subtree: nothing changes.
            _ => return Ok(Child::Node(id)),
        };
        self.slots[id as usize].cached = None;
        self.slots[id as usize].node = replaced;
        Ok(Child::Node(id))
    }

    /// The node for `prefix` followed by `child` (found at `path`), folding a leaf or extension
    /// child into one node.
    fn merge(
        &mut self,
        prefix: &[u8],
        child: Child,
        path: &Path,
    ) -> Result<Option<Node>, TrieError> {
        let Some(cid) = self.load(child, path)? else { return Ok(None) };
        let joined = |p: &Path| {
            let mut out = Path::of(prefix);
            out.extend(p.get());
            out
        };
        Ok(Some(match self.slots[cid as usize].node {
            Node::Leaf { path: p, value } => Node::Leaf { path: joined(&p), value },
            Node::Ext { path: p, child: c } => Node::Ext { path: joined(&p), child: c },
            Node::Branch { .. } => Node::Ext { path: Path::of(prefix), child: Child::Node(cid) },
        }))
    }

    pub fn root_hash(&mut self) -> H256 {
        hash_tries(&mut [&mut *self]);
        self.root_ref()
    }

    /// The root hash, once every touched node is hashed.
    fn root_ref(&self) -> H256 {
        let r = match self.root {
            Child::Empty => return *EMPTY_TRIE_HASH,
            Child::Unloaded(r) => r,
            Child::Node(id) => match self.slots[id as usize].cached {
                Some(r) => r,
                None => return *EMPTY_TRIE_HASH,
            },
        };
        match r {
            Ref::Hash(h) => h,
            Ref::Inline { len, data } => H256(keccak_hash(&data[..len as usize])),
        }
    }

    /// Adds every node a write touched, by depth, to `levels` as `(trie, id)`.
    fn collect_dirty(&self, trie: usize, levels: &mut Vec<Vec<(usize, Id)>>) {
        let Child::Node(root) = self.root else { return };
        let mut stack = vec![(root, 0usize)];
        while let Some((id, depth)) = stack.pop() {
            if self.slots[id as usize].cached.is_some() {
                continue;
            }
            if levels.len() <= depth {
                levels.resize_with(depth + 1, Vec::new);
            }
            levels[depth].push((trie, id));
            match &self.slots[id as usize].node {
                Node::Ext { child: Child::Node(c), .. } => stack.push((*c, depth + 1)),
                Node::Branch { children } => {
                    for c in children {
                        if let Child::Node(c) = c {
                            stack.push((*c, depth + 1));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Appends the node's encoding to `out`; its children are already hashed.
    fn encode(&self, id: Id, payload: &mut Vec<u8>, out: &mut Vec<u8>) {
        payload.clear();
        let reference = |c: &Child| -> Option<Ref> {
            match c {
                Child::Empty => None,
                Child::Unloaded(r) => Some(*r),
                Child::Node(id) => self.slots[*id as usize].cached,
            }
        };
        match &self.slots[id as usize].node {
            Node::Leaf { path, value } => {
                put_compact(payload, path.get(), true);
                put_bytes(payload, value.get());
            }
            Node::Ext { path, child } => {
                put_compact(payload, path.get(), false);
                put_ref(payload, reference(child));
            }
            Node::Branch { children } => {
                for c in children {
                    put_ref(payload, reference(c));
                }
                payload.push(0x80);
            }
        }
        put_list(out, payload);
    }
}

/// Hashes every touched node across `tries` a depth at a time from the deepest, so each depth
/// is one batch for the multi-lane hash however the nodes are spread over tries.
fn hash_tries(tries: &mut [&mut Mpt]) {
    let mut levels: Vec<Vec<(usize, Id)>> = Vec::new();
    for (ix, trie) in tries.iter().enumerate() {
        trie.collect_dirty(ix, &mut levels);
    }
    let mut payload = Vec::with_capacity(544);
    let mut encoded = Vec::with_capacity(8192);
    let mut ends: Vec<usize> = Vec::new();
    let mut hashes: Vec<[u8; 32]> = Vec::new();
    for level in levels.iter().rev() {
        encoded.clear();
        ends.clear();
        for &(trie, id) in level {
            tries[trie].encode(id, &mut payload, &mut encoded);
            ends.push(encoded.len());
        }
        let mut start = 0;
        let spans: Vec<&[u8]> = ends
            .iter()
            .map(|&end| {
                let span = &encoded[start..end];
                start = end;
                span
            })
            .collect();
        let long: Vec<&[u8]> = spans.iter().copied().filter(|s| s.len() >= 32).collect();
        crate::engine::keccak8::hash_many(&long, &mut hashes);
        let mut hashed = hashes.iter();
        for (&(trie, id), span) in level.iter().zip(&spans) {
            let r = if span.len() < 32 {
                let mut data = [0; 31];
                data[..span.len()].copy_from_slice(span);
                Ref::Inline { len: span.len() as u8, data }
            } else {
                Ref::Hash(H256(*hashed.next().unwrap_or(&[0; 32])))
            };
            tries[trie].slots[id as usize].cached = Some(r);
        }
    }
}

/// Hex-prefix encoding of a nibble path, as an RLP string.
fn put_compact(out: &mut Vec<u8>, path: &[u8], leaf: bool) {
    let flag = if leaf { 2 } else { 0 } + (path.len() % 2) as u8;
    let mut bytes = [0u8; 33];
    let rest = if path.len() % 2 == 1 {
        bytes[0] = (flag << 4) | path[0];
        &path[1..]
    } else {
        bytes[0] = flag << 4;
        path
    };
    for (i, pair) in rest.chunks(2).enumerate() {
        bytes[1 + i] = (pair[0] << 4) | pair[1];
    }
    put_bytes(out, &bytes[..1 + rest.len() / 2]);
}

fn put_len(out: &mut Vec<u8>, len: usize, short: u8, long: u8) {
    if len < 56 {
        out.push(short + len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(long + (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        out.push(bytes[0]);
    } else {
        put_len(out, bytes.len(), 0x80, 0xb7);
        out.extend_from_slice(bytes);
    }
}

fn put_list(out: &mut Vec<u8>, payload: &[u8]) {
    put_len(out, payload.len(), 0xc0, 0xf7);
    out.extend_from_slice(payload);
}

fn put_ref(out: &mut Vec<u8>, r: Option<Ref>) {
    match r {
        None => out.push(0x80),
        Some(Ref::Hash(h)) => put_bytes(out, h.as_bytes()),
        Some(Ref::Inline { len, data }) => out.extend_from_slice(&data[..len as usize]),
    }
}

/// One RLP item at the start of `data`: whether it is a list, its payload, and its full length.
fn item(data: &[u8]) -> Result<(bool, &[u8], usize), TrieError> {
    let bad = || TrieError::Verify("bad rlp".into());
    let first = *data.first().ok_or_else(bad)?;
    let (list, header, len) = match first {
        0x00..=0x7f => return Ok((false, &data[..1], 1)),
        0x80..=0xb7 => (false, 1, (first - 0x80) as usize),
        0xb8..=0xbf => {
            let n = (first - 0xb7) as usize;
            (
                false,
                1 + n,
                data.get(1..1 + n).ok_or_else(bad)?.iter().fold(0, |a, b| a << 8 | *b as usize),
            )
        }
        0xc0..=0xf7 => (true, 1, (first - 0xc0) as usize),
        _ => {
            let n = (first - 0xf7) as usize;
            (
                true,
                1 + n,
                data.get(1..1 + n).ok_or_else(bad)?.iter().fold(0, |a, b| a << 8 | *b as usize),
            )
        }
    };
    let payload = data.get(header..header + len).ok_or_else(bad)?;
    Ok((list, payload, header + len))
}

fn child_ref(data: &[u8]) -> Result<Child, TrieError> {
    let (list, payload, len) = item(data)?;
    Ok(if list {
        if len > 31 {
            return Err(TrieError::Verify("inline node over 31 bytes".into()));
        }
        let mut inline = [0; 31];
        inline[..len].copy_from_slice(&data[..len]);
        Child::Unloaded(Ref::Inline { len: len as u8, data: inline })
    } else if payload.is_empty() {
        Child::Empty
    } else {
        Child::Unloaded(Ref::Hash(H256::from_slice(payload)))
    })
}

fn decode(encoded: &[u8]) -> Result<Node, TrieError> {
    let (_, mut payload, _) = item(encoded)?;
    let mut items: [&[u8]; 17] = [&[]; 17];
    let mut n = 0;
    while !payload.is_empty() {
        let (_, _, len) = item(payload)?;
        if n == 17 {
            return Err(TrieError::Verify("bad node".into()));
        }
        items[n] = &payload[..len];
        n += 1;
        payload = &payload[len..];
    }
    match n {
        17 => {
            let mut children = [Child::Empty; 16];
            for (c, data) in children.iter_mut().zip(&items) {
                *c = child_ref(data)?;
            }
            Ok(Node::Branch { children })
        }
        2 => {
            let (_, compact, _) = item(items[0])?;
            let flag = compact.first().map(|b| b >> 4).unwrap_or_default();
            if compact.len() > 33 {
                return Err(TrieError::Verify("path over 64 nibbles".into()));
            }
            let mut path = Path::EMPTY;
            if flag & 1 == 1 {
                path.push(compact[0] & 0x0f);
            }
            for b in compact.iter().skip(1) {
                path.push(b >> 4);
                path.push(b & 0x0f);
            }
            if flag & 2 == 2 {
                let (_, value, _) = item(items[1])?;
                Ok(Node::Leaf { path, value: Value::of(value)? })
            } else {
                Ok(Node::Ext { path, child: child_ref(items[1])? })
            }
        }
        _ => Err(TrieError::Verify("bad node".into())),
    }
}

const PARALLEL_TRIES: usize = 64;

static TRIE_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .thread_name(|i| format!("b-trie-{i}"))
        .build()
        .expect("b trie pool")
});

/// The state: the account trie plus each touched account's storage trie.
pub struct State {
    accounts: Mpt,
    storage: FxHashMap<H256, Mpt>,
    open_storage: Box<dyn Fn(H256) -> Result<Box<dyn TrieDB>, TrieError> + Send + Sync>,
    shared: Shared,
}

impl State {
    pub fn open(
        accounts: Box<dyn TrieDB>,
        root: H256,
        open_storage: Box<dyn Fn(H256) -> Result<Box<dyn TrieDB>, TrieError> + Send + Sync>,
    ) -> Self {
        let shared = shared(root);
        State {
            accounts: Mpt::open(accounts, root, None, shared.clone()),
            storage: FxHashMap::default(),
            open_storage,
            shared,
        }
    }

    /// Applies `updates` and returns the new state root. Storage writes go first, then every
    /// touched storage trie is hashed in parallel, then the account trie.
    pub fn apply(&mut self, updates: &[AccountUpdate]) -> Result<H256, TrieError> {
        let mut accounts: Vec<(H256, &AccountUpdate, AccountState)> =
            Vec::with_capacity(updates.len());
        for update in updates {
            let hashed = H256(keccak_hash(update.address.as_bytes()));
            if update.removed {
                self.storage.remove(&hashed);
                accounts.push((hashed, update, AccountState::default()));
                continue;
            }
            let mut account = match self.accounts.get(hashed.as_bytes())? {
                Some(encoded) => AccountState::decode(encoded.get())
                    .map_err(|e| TrieError::Verify(e.to_string()))?,
                None => AccountState::default(),
            };
            if update.removed_storage {
                account.storage_root = *EMPTY_TRIE_HASH;
                self.storage.remove(&hashed);
            }
            if !update.added_storage.is_empty() {
                let trie = match self.storage.entry(hashed) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Mpt::open(
                        (self.open_storage)(hashed)?,
                        account.storage_root,
                        Some(hashed),
                        self.shared.clone(),
                    )),
                };
                for (slot, value) in &update.added_storage {
                    let key = keccak_hash(slot.as_bytes());
                    let value = match *value != U256::zero() {
                        true => Some(Value::of(&value.encode_to_vec())?),
                        false => None,
                    };
                    trie.set(&key, value)?;
                }
            }
            accounts.push((hashed, update, account));
        }
        let touched: rustc_hash::FxHashSet<H256> = updates
            .iter()
            .filter(|u| !u.removed && !u.added_storage.is_empty())
            .map(|u| H256(keccak_hash(u.address.as_bytes())))
            .collect();
        let mut storage: Vec<(H256, &mut Mpt)> = self
            .storage
            .iter_mut()
            .filter(|(hashed, _)| touched.contains(*hashed))
            .map(|(h, t)| (*h, t))
            .collect();
        let mut tries: Vec<&mut Mpt> = storage.iter_mut().map(|(_, t)| &mut **t).collect();
        // A large delta touches many storage tries: hash them in groups across cores, each group
        // still batched by depth.
        if tries.len() >= PARALLEL_TRIES {
            let group = tries.len().div_ceil(8);
            TRIE_POOL.install(|| {
                use rayon::prelude::*;
                tries.par_chunks_mut(group).for_each(hash_tries);
            });
        } else {
            hash_tries(&mut tries);
        }
        let roots: FxHashMap<H256, H256> =
            storage.iter().map(|(h, t)| (*h, t.root_ref())).collect();
        for (hashed, update, mut account) in accounts {
            if update.removed {
                self.accounts.set(hashed.as_bytes(), None)?;
                continue;
            }
            if let Some(info) = &update.info {
                account.nonce = info.nonce;
                account.balance = info.balance;
                account.code_hash = info.code_hash;
            }
            if let Some(root) = roots.get(&hashed) {
                account.storage_root = *root;
            }
            self.accounts.set(hashed.as_bytes(), Some(Value::of(&account.encode_to_vec())?))?;
        }
        Ok(self.accounts.root_hash())
    }
}
