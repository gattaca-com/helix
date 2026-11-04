use ethrex_common::{H256, constants::EMPTY_TRIE_HASH};
use ethrex_trie::{Nibbles, TrieDB};
use rustc_hash::FxHashMap;

use crate::merge::rlp::{HashBatch, NodeRef, put_branch, put_ext, put_leaf};

pub const FULL: &str = "trie_full";
const MISSING: &str = "trie_missing_node";
const DECODE: &str = "trie_decode";
const STORE: &str = "trie_store";

// Child id: 0 empty; OWN = builder arena; HASH = index into `by_hash`, else into `nodes`.
const OWN: u32 = 1 << 31;
const HASH: u32 = 1 << 30;
const INDEX: u32 = HASH - 1;
const VALUE_MAX: usize = 128;
const KEY_NIBBLES: usize = 64;
const DECODE_MAX_NODES: usize = 33;

#[derive(Clone, Copy)]
struct NibblePath {
    len: u8,
    nib: [u8; KEY_NIBBLES],
}

impl NibblePath {
    fn new(nibbles: &[u8]) -> NibblePath {
        NibblePath::join(&[], nibbles)
    }

    fn join(a: &[u8], b: &[u8]) -> NibblePath {
        debug_assert!(a.len() + b.len() <= KEY_NIBBLES);
        let mut path = NibblePath { len: (a.len() + b.len()) as u8, nib: [0; KEY_NIBBLES] };
        path.nib[..a.len()].copy_from_slice(a);
        path.nib[a.len()..a.len() + b.len()].copy_from_slice(b);
        path
    }

    fn as_slice(&self) -> &[u8] {
        &self.nib[..self.len as usize]
    }
}

#[derive(Clone, Copy)]
struct LeafValue {
    len: u8,
    data: [u8; VALUE_MAX],
}

impl LeafValue {
    fn new(bytes: &[u8]) -> Result<LeafValue, &'static str> {
        if bytes.len() > VALUE_MAX {
            return Err(DECODE);
        }
        let mut value = LeafValue { len: bytes.len() as u8, data: [0; VALUE_MAX] };
        value.data[..bytes.len()].copy_from_slice(bytes);
        Ok(value)
    }

    fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum TrieNode {
    Leaf { path: NibblePath, value: LeafValue },
    Ext { path: NibblePath, child: u32 },
    Branch { children: [u32; 16] },
}

#[derive(Clone, Copy)]
struct ArenaNode {
    node: TrieNode,
    // Empty when the node changed since the last commit.
    cached_ref: NodeRef,
}

pub struct NodeArena {
    tag: u32,
    nodes: Vec<ArenaNode>,
    by_hash: Vec<(H256, u32)>,
    root_ids: FxHashMap<H256, u32>,
    pub(super) loaded: FxHashMap<H256, u32>,
    pub spilled: u32,
}

impl NodeArena {
    pub fn new(own: bool, slots: usize, hashes: usize) -> NodeArena {
        let mut arena = NodeArena {
            tag: if own { OWN } else { 0 },
            nodes: Vec::with_capacity(slots),
            by_hash: Vec::with_capacity(hashes),
            root_ids: FxHashMap::default(),
            loaded: FxHashMap::with_capacity_and_hasher(
                if own { 0 } else { slots },
                Default::default(),
            ),
            spilled: 0,
        };
        arena.clear();
        arena
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.by_hash.clear();
        self.root_ids.clear();
        self.loaded.clear();
        self.spilled = 0;
        let empty = TrieNode::Branch { children: [0; 16] };
        self.nodes.push(ArenaNode { node: empty, cached_ref: NodeRef::default() });
        self.by_hash.push((H256::zero(), 0));
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn trim(&mut self) {
        if 2 * self.nodes.len() > self.nodes.capacity() ||
            2 * self.by_hash.len() > self.by_hash.capacity()
        {
            self.clear();
        }
    }

    fn has_room(&self) -> bool {
        self.nodes.len() + DECODE_MAX_NODES <= self.nodes.capacity() &&
            self.by_hash.len() + DECODE_MAX_NODES <= self.by_hash.capacity()
    }
}

#[derive(Default)]
pub struct TrieHasher {
    stack: Vec<(u32, usize)>,
    levels: Vec<Vec<u32>>,
    batch: HashBatch,
}

pub struct Tries<'a> {
    pub parent: &'a mut NodeArena,
    pub own: &'a mut NodeArena,
}

impl Tries<'_> {
    fn arena(&self, id: u32) -> &NodeArena {
        if id & OWN != 0 { self.own } else { self.parent }
    }

    fn arena_mut(&mut self, tag: u32) -> &mut NodeArena {
        if tag & OWN != 0 { self.own } else { self.parent }
    }

    fn node(&self, id: u32) -> &ArenaNode {
        let slots = &self.arena(id).nodes;
        debug_assert!(id & HASH == 0 && ((id & INDEX) as usize) < slots.len());
        &slots[(id & INDEX) as usize]
    }

    pub fn open_root(&mut self, hash: H256) -> u32 {
        if hash == *EMPTY_TRIE_HASH {
            return 0;
        }
        for arena in [&*self.parent, &*self.own] {
            if let Some(&id) = arena.root_ids.get(&hash) {
                return id;
            }
        }
        let arena = match self.parent.has_room() {
            true => &mut *self.parent,
            false => {
                self.own.spilled += 1;
                &mut *self.own
            }
        };
        let id = arena.tag | HASH | arena.by_hash.len() as u32;
        arena.by_hash.push((hash, 0));
        arena.root_ids.insert(hash, id);
        id
    }

    pub fn put(
        &mut self,
        db: &dyn TrieDB,
        root: u32,
        key: &[u8; 32],
        value: Option<&[u8]>,
    ) -> Result<u32, &'static str> {
        let value = value.map(LeafValue::new).transpose()?;
        Ok(self.put_at(db, root, &to_nibbles(key), 0, value.as_ref())?.0)
    }

    fn put_at(
        &mut self,
        db: &dyn TrieDB,
        id: u32,
        key: &[u8; KEY_NIBBLES],
        depth: usize,
        value: Option<&LeafValue>,
    ) -> Result<(u32, bool), &'static str> {
        let id = self.load(db, id, &key[..depth])?;
        let rest = &key[depth..];
        if id == 0 {
            return match value {
                Some(&value) => {
                    Ok((self.alloc(TrieNode::Leaf { path: NibblePath::new(rest), value })?, true))
                }
                None => Ok((0, false)),
            };
        }
        match self.node(id).node {
            TrieNode::Leaf { path, value: old } => {
                if path.as_slice() == rest {
                    return match value {
                        Some(&value) => {
                            Ok((self.rewrite(id, TrieNode::Leaf { path, value })?, true))
                        }
                        None => Ok((0, true)),
                    };
                }
                let Some(&value) = value else { return Ok((id, false)) };
                // Keys are all KEY_NIBBLES nibbles, so the paths differ before either ends.
                let c = common_prefix(path.as_slice(), rest);
                let old = self.rewrite(id, TrieNode::Leaf {
                    path: NibblePath::new(&path.as_slice()[c + 1..]),
                    value: old,
                })?;
                let new =
                    self.alloc(TrieNode::Leaf { path: NibblePath::new(&rest[c + 1..]), value })?;
                let mut children = [0; 16];
                children[path.nib[c] as usize] = old;
                children[rest[c] as usize] = new;
                Ok((self.branch_under(&rest[..c], children)?, true))
            }
            TrieNode::Ext { path, child } => {
                let c = common_prefix(path.as_slice(), rest);
                if c == path.len as usize {
                    let (new, changed) = self.put_at(db, child, key, depth + c, value)?;
                    if !changed {
                        return Ok((id, false));
                    }
                    return Ok((self.prefix_child(id, path.as_slice(), new)?, true));
                }
                let Some(&value) = value else { return Ok((id, false)) };
                let tail = match path.len as usize - c - 1 {
                    0 => child,
                    _ => self.rewrite(id, TrieNode::Ext {
                        path: NibblePath::new(&path.as_slice()[c + 1..]),
                        child,
                    })?,
                };
                let new =
                    self.alloc(TrieNode::Leaf { path: NibblePath::new(&rest[c + 1..]), value })?;
                let mut children = [0; 16];
                children[path.nib[c] as usize] = tail;
                children[rest[c] as usize] = new;
                Ok((self.branch_under(&rest[..c], children)?, true))
            }
            TrieNode::Branch { mut children } => {
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
                        let mut prefix = [0; KEY_NIBBLES];
                        prefix[..depth].copy_from_slice(&key[..depth]);
                        prefix[depth] = i as u8;
                        let child = self.load(db, child, &prefix[..depth + 1])?;
                        self.prefix_child(id, &[i as u8], child)?
                    }
                    (None, _) => 0,
                    _ => self.rewrite(id, TrieNode::Branch { children })?,
                };
                Ok((id, true))
            }
        }
    }

    fn prefix_child(&mut self, id: u32, prefix: &[u8], child: u32) -> Result<u32, &'static str> {
        if child == 0 {
            return Ok(0);
        }
        let node = match self.node(child).node {
            TrieNode::Leaf { path, value } => {
                TrieNode::Leaf { path: NibblePath::join(prefix, path.as_slice()), value }
            }
            TrieNode::Ext { path, child } => {
                TrieNode::Ext { path: NibblePath::join(prefix, path.as_slice()), child }
            }
            TrieNode::Branch { .. } => TrieNode::Ext { path: NibblePath::new(prefix), child },
        };
        self.rewrite(id, node)
    }

    fn branch_under(&mut self, prefix: &[u8], children: [u32; 16]) -> Result<u32, &'static str> {
        let branch = self.alloc(TrieNode::Branch { children })?;
        match prefix.is_empty() {
            true => Ok(branch),
            false => self.alloc(TrieNode::Ext { path: NibblePath::new(prefix), child: branch }),
        }
    }

    fn rewrite(&mut self, id: u32, node: TrieNode) -> Result<u32, &'static str> {
        if id & OWN == 0 {
            return self.alloc(node);
        }
        self.own.nodes[(id & INDEX) as usize] = ArenaNode { node, cached_ref: NodeRef::default() };
        Ok(id)
    }

    fn alloc(&mut self, node: TrieNode) -> Result<u32, &'static str> {
        let slots = &mut self.own.nodes;
        if slots.len() == slots.capacity() {
            return Err(FULL);
        }
        slots.push(ArenaNode { node, cached_ref: NodeRef::default() });
        Ok(OWN | (slots.len() - 1) as u32)
    }

    fn load(&mut self, db: &dyn TrieDB, id: u32, prefix: &[u8]) -> Result<u32, &'static str> {
        if id & HASH == 0 {
            return Ok(id);
        }
        let ix = (id & INDEX) as usize;
        let (hash, loaded) = self.arena(id).by_hash[ix];
        if loaded != 0 {
            return Ok(loaded);
        }
        if let Some(&node) = self.parent.loaded.get(&hash) {
            self.arena_mut(id).by_hash[ix].1 = node;
            return Ok(node);
        }
        let encoded = db
            .get(Nibbles::from_hex(prefix.to_vec()))
            .map_err(|_| STORE)?
            .filter(|e| !e.is_empty())
            .ok_or(MISSING)?;
        let to = if id & OWN == 0 && self.parent.has_room() { 0 } else { OWN };
        if to & OWN != 0 && !self.own.has_room() {
            return Err(FULL);
        }
        let node = self.decode(to, &encoded, NodeRef { len: 32, data: hash.0 })?;
        // The parent arena is shared, so it never points into a builder's.
        match to == id & OWN {
            true => self.arena_mut(id).by_hash[ix].1 = node,
            false => self.own.spilled += 1,
        }
        if to == 0 {
            self.parent.loaded.insert(hash, node);
        }
        Ok(node)
    }

    fn decode(
        &mut self,
        to: u32,
        encoded: &[u8],
        cached_ref: NodeRef,
    ) -> Result<u32, &'static str> {
        let (list, mut payload, _) = rlp_item(encoded)?;
        if !list {
            return Err(DECODE);
        }
        let mut items = [(false, &[][..], &[][..]); 17];
        let mut n = 0;
        while !payload.is_empty() {
            let (list, inner, len) = rlp_item(payload)?;
            *items.get_mut(n).ok_or(DECODE)? = (list, inner, &payload[..len]);
            payload = &payload[len..];
            n += 1;
        }
        let node = match n {
            17 => {
                if items[16].0 || !items[16].1.is_empty() {
                    return Err(DECODE);
                }
                let mut children = [0; 16];
                for (child, item) in children.iter_mut().zip(&items) {
                    *child = self.decode_child(to, *item)?;
                }
                TrieNode::Branch { children }
            }
            2 => {
                let (list, compact, _) = items[0];
                let (&first, rest) = compact.split_first().ok_or(DECODE)?;
                if list || first >> 4 > 3 {
                    return Err(DECODE);
                }
                let mut path = NibblePath { len: 0, nib: [0; KEY_NIBBLES] };
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
                    true if !items[1].0 => {
                        TrieNode::Leaf { path, value: LeafValue::new(items[1].1)? }
                    }
                    true => return Err(DECODE),
                    false => TrieNode::Ext { path, child: self.decode_child(to, items[1])? },
                }
            }
            _ => return Err(DECODE),
        };
        let arena = self.arena_mut(to);
        arena.nodes.push(ArenaNode { node, cached_ref });
        Ok(to | (arena.nodes.len() - 1) as u32)
    }

    fn decode_child(&mut self, to: u32, item: (bool, &[u8], &[u8])) -> Result<u32, &'static str> {
        match item {
            (true, _, whole) => {
                let mut r = NodeRef { len: whole.len() as u8, data: [0; 32] };
                r.data.get_mut(..whole.len()).ok_or(DECODE)?.copy_from_slice(whole);
                self.decode(to, whole, r)
            }
            (false, [], _) => Ok(0),
            (false, hash, _) if hash.len() == 32 => {
                let arena = self.arena_mut(to);
                arena.by_hash.push((H256::from_slice(hash), 0));
                Ok(to | HASH | (arena.by_hash.len() - 1) as u32)
            }
            _ => Err(DECODE),
        }
    }

    fn node_ref(&self, id: u32) -> Option<NodeRef> {
        match id {
            0 => None,
            _ if id & HASH != 0 => {
                let (hash, _) = self.arena(id).by_hash[(id & INDEX) as usize];
                Some(NodeRef { len: 32, data: hash.0 })
            }
            _ => Some(self.node(id).cached_ref),
        }
    }

    pub fn commit(&mut self, h: &mut TrieHasher, roots: &[u32], out: &mut Vec<H256>) {
        for level in &mut h.levels {
            level.clear();
        }
        for &root in roots {
            h.stack.push((root, 0));
        }
        while let Some((id, depth)) = h.stack.pop() {
            if id & OWN == 0 || id & HASH != 0 || self.node(id).cached_ref.len != 0 {
                continue;
            }
            if h.levels.len() <= depth {
                h.levels.resize_with(depth + 1, Vec::new);
            }
            h.levels[depth].push(id);
            match self.node(id).node {
                TrieNode::Leaf { .. } => {}
                TrieNode::Ext { child, .. } => h.stack.push((child, depth + 1)),
                TrieNode::Branch { children } => {
                    h.stack.extend(children.iter().map(|&c| (c, depth + 1)));
                }
            }
        }
        for depth in (0..h.levels.len()).rev() {
            h.batch.clear();
            for i in 0..h.levels[depth].len() {
                let id = h.levels[depth][i];
                let at = h.batch.enc.len();
                self.encode(id, &mut h.batch.enc);
                if let Some(r) = h.batch.push(id, at) {
                    self.own.nodes[(id & INDEX) as usize].cached_ref = r;
                }
            }
            for (id, r) in h.batch.hash() {
                self.own.nodes[(id & INDEX) as usize].cached_ref = r;
            }
        }
        out.clear();
        out.extend(
            roots.iter().map(|&root| self.node_ref(root).map_or(*EMPTY_TRIE_HASH, |r| r.root())),
        );
    }

    fn encode(&self, id: u32, enc: &mut Vec<u8>) {
        match self.node(id).node {
            TrieNode::Leaf { path, value } => put_leaf(enc, path.as_slice(), value.as_slice()),
            TrieNode::Ext { path, child } => {
                put_ext(enc, path.as_slice(), &self.node_ref(child).unwrap_or_default());
            }
            TrieNode::Branch { children } => put_branch(enc, &children.map(|c| self.node_ref(c))),
        }
    }
}

fn to_nibbles(key: &[u8; 32]) -> [u8; KEY_NIBBLES] {
    std::array::from_fn(|i| if i % 2 == 0 { key[i / 2] >> 4 } else { key[i / 2] & 0x0f })
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn rlp_item(data: &[u8]) -> Result<(bool, &[u8], usize), &'static str> {
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
