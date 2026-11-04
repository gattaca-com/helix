use ethrex_common::H256;
use ethrex_crypto::keccak::keccak_hash;

use crate::merge::keccak;

#[derive(Clone, Copy, Default)]
pub struct NodeRef {
    pub len: u8,
    pub data: [u8; 32],
}

impl NodeRef {
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    pub fn root(&self) -> H256 {
        match self.len {
            32 => H256(self.data),
            _ => H256(keccak_hash(self.as_slice())),
        }
    }
}

// Encodes one trie level into `enc`, then hashes its nodes of 32+ bytes as one batch.
// Per level: `clear`, then for each node encode at `enc.len()` and `push`, then `hash`.
#[derive(Default)]
pub struct HashBatch {
    pub enc: Vec<u8>,
    // (node id, at, len) in `enc`.
    long: Vec<(u32, u32, u32)>,
    hashes: Vec<[u8; 32]>,
}

impl HashBatch {
    pub fn clear(&mut self) {
        self.enc.clear();
        self.long.clear();
    }

    // Returns a short encoding inline and drops it from `enc`; a long one waits for `hash`.
    pub fn push(&mut self, id: u32, at: usize) -> Option<NodeRef> {
        let len = self.enc.len() - at;
        if len >= 32 {
            self.long.push((id, at as u32, len as u32));
            return None;
        }
        let mut r = NodeRef { len: len as u8, data: [0; 32] };
        r.data[..len].copy_from_slice(&self.enc[at..]);
        self.enc.truncate(at);
        Some(r)
    }

    pub fn hash(&mut self) -> impl Iterator<Item = (u32, NodeRef)> + '_ {
        self.long.sort_unstable_by_key(|&(_, _, len)| len);
        self.hashes.resize(self.long.len(), [0; 32]);
        keccak::hash_many(&self.enc, &self.long, &mut self.hashes);
        let hashed = self.long.iter().zip(&self.hashes);
        hashed.map(|(&(id, ..), hash)| (id, NodeRef { len: 32, data: *hash }))
    }
}

pub fn put_leaf(out: &mut Vec<u8>, path: &[u8], value: &[u8]) {
    put_list_header(out, compact_len(path) + bytes_len(value));
    put_compact(out, path, true);
    put_bytes(out, value);
}

pub fn put_ext(out: &mut Vec<u8>, path: &[u8], child: &NodeRef) {
    put_list_header(out, compact_len(path) + ref_len(child));
    put_compact(out, path, false);
    put_ref(out, child);
}

pub fn put_branch(out: &mut Vec<u8>, children: &[Option<NodeRef>; 16]) {
    let len: usize = children.iter().map(|c| c.as_ref().map_or(1, ref_len)).sum();
    put_list_header(out, len + 1);
    for child in children {
        match child {
            Some(r) => put_ref(out, r),
            None => out.push(0x80),
        }
    }
    out.push(0x80);
}

fn header_len(len: usize) -> usize {
    match len {
        0..56 => 1,
        _ => 1 + 8 - len.leading_zeros() as usize / 8,
    }
}

fn put_header(out: &mut Vec<u8>, base: u8, len: usize) {
    if len < 56 {
        out.push(base + len as u8);
        return;
    }
    let n = 8 - len.leading_zeros() as usize / 8;
    out.push(base + 55 + n as u8);
    out.extend_from_slice(&len.to_be_bytes()[8 - n..]);
}

fn put_list_header(out: &mut Vec<u8>, len: usize) {
    put_header(out, 0xc0, len);
}

fn bytes_len(bytes: &[u8]) -> usize {
    match bytes {
        [b] if *b < 0x80 => 1,
        _ => header_len(bytes.len()) + bytes.len(),
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if let [b] = bytes &&
        *b < 0x80
    {
        out.push(*b);
        return;
    }
    put_header(out, 0x80, bytes.len());
    out.extend_from_slice(bytes);
}

fn compact_len(path: &[u8]) -> usize {
    let n = path.len() / 2 + 1;
    // The flag nibble is at most 3, so a lone compact byte is its own RLP string.
    if n == 1 { 1 } else { 1 + n }
}

fn put_compact(out: &mut Vec<u8>, path: &[u8], leaf: bool) {
    let flag = 2 * u8::from(leaf) + (path.len() % 2) as u8;
    let n = path.len() / 2 + 1;
    let (first, rest) = match path.len() % 2 {
        1 => ((flag << 4) | path[0], &path[1..]),
        _ => (flag << 4, path),
    };
    if n == 1 {
        out.push(first);
        return;
    }
    out.push(0x80 + n as u8);
    out.push(first);
    out.extend(rest.chunks_exact(2).map(|p| (p[0] << 4) | p[1]));
}

fn ref_len(r: &NodeRef) -> usize {
    match r.len {
        32 => 33,
        len => len as usize,
    }
}

fn put_ref(out: &mut Vec<u8>, r: &NodeRef) {
    if r.len == 32 {
        out.push(0xa0);
    }
    out.extend_from_slice(r.as_slice());
}
