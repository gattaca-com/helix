//! Roots of a block's ordered lists (txs, receipts): the trie over `rlp(index) -> value`. Nodes go
//! into a flat array and are encoded and hashed a height at a time, leaves first, so each height's
//! hashes are one batch. Buffers keep their capacity across calls.
//!
//! No key is a prefix of another (`rlp(index)` is self-delimiting), so branches never hold values.

use ethrex_common::{H256, constants::EMPTY_TRIE_HASH};
use ethrex_crypto::keccak::keccak_hash;

use crate::merge::{
    keccak,
    rlp::{Ref, bytes_len, compact_len, put_bytes, put_compact, put_list_header, put_ref, ref_len},
};

const NONE: u32 = u32::MAX;
/// `rlp(index)` is at most 5 bytes for a `u32` index.
const NIBBLES: usize = 10;
/// A node is at most one level per nibble above its leaves.
const HEIGHTS: usize = NIBBLES + 1;

#[derive(Clone, Copy)]
struct Key {
    nib: [u8; NIBBLES],
    len: u8,
    index: u32,
}

impl Key {
    fn of(index: u32) -> Key {
        let mut bytes = [0u8; 5];
        let len = match index {
            0 => {
                bytes[0] = 0x80;
                1
            }
            1..=0x7f => {
                bytes[0] = index as u8;
                1
            }
            _ => {
                let n = 4 - index.leading_zeros() as usize / 8;
                bytes[0] = 0x80 + n as u8;
                bytes[1..=n].copy_from_slice(&index.to_be_bytes()[4 - n..]);
                n + 1
            }
        };
        let mut nib = [0u8; NIBBLES];
        for (i, b) in bytes[..len].iter().enumerate() {
            nib[2 * i] = b >> 4;
            nib[2 * i + 1] = b & 0x0f;
        }
        Key { nib, len: 2 * len as u8, index }
    }

    fn rest(&self, depth: usize) -> &[u8] {
        &self.nib[depth..self.len as usize]
    }
}

#[derive(Clone, Copy)]
enum Node {
    Leaf { key: u32, depth: u8 },
    Ext { key: u32, depth: u8, len: u8, child: u32 },
    Branch { children: [u32; 16] },
}

#[derive(Default)]
pub struct Lists {
    keys: Vec<Key>,
    nodes: Vec<Node>,
    /// By node.
    refs: Vec<Ref>,
    by_height: [Vec<u32>; HEIGHTS],
    /// One height's encodings.
    enc: Vec<u8>,
    /// Nodes of the height whose encoding is 32 bytes or more: id and span in `enc`.
    long: Vec<(u32, u32, u32)>,
    /// `long`'s spans and hashes, by length.
    spans: Vec<(u32, u32)>,
    hashes: Vec<[u8; 32]>,
}

impl Lists {
    /// The root over `value(0..n)`.
    pub fn root<'a>(&mut self, n: usize, value: impl Fn(usize) -> &'a [u8]) -> H256 {
        if n == 0 {
            return *EMPTY_TRIE_HASH;
        }
        self.keys.clear();
        self.keys.extend((0..n as u32).map(Key::of));
        self.keys.sort_unstable_by(|a, b| a.nib[..a.len as usize].cmp(&b.nib[..b.len as usize]));
        self.nodes.clear();
        for ids in &mut self.by_height {
            ids.clear();
        }
        let (root, _) = self.build(0, n, 0);
        self.refs.clear();
        self.refs.resize(self.nodes.len(), Ref::default());
        for h in 0..HEIGHTS {
            if self.by_height[h].is_empty() {
                continue;
            }
            self.enc.clear();
            self.long.clear();
            for i in 0..self.by_height[h].len() {
                let id = self.by_height[h][i];
                let at = self.enc.len();
                self.encode(id, &value);
                let len = self.enc.len() - at;
                if len < 32 {
                    let r = &mut self.refs[id as usize];
                    r.len = len as u8;
                    r.data[..len].copy_from_slice(&self.enc[at..]);
                    self.enc.truncate(at);
                } else {
                    self.long.push((id, at as u32, len as u32));
                }
            }
            self.long.sort_unstable_by_key(|&(_, _, len)| len);
            self.spans.clear();
            self.spans.extend(self.long.iter().map(|&(_, at, len)| (at, len)));
            self.hashes.resize(self.spans.len(), [0; 32]);
            keccak::many(&self.enc, &self.spans, &mut self.hashes);
            for (&(id, ..), hash) in self.long.iter().zip(&self.hashes) {
                self.refs[id as usize] = Ref { len: 32, data: *hash };
            }
        }
        let r = self.refs[root as usize];
        match r.len {
            32 => H256(r.data),
            _ => H256(keccak_hash(r.get())),
        }
    }

    /// The node over sorted keys `lo..hi`, which share their first `depth` nibbles, and its height.
    fn build(&mut self, lo: usize, hi: usize, depth: usize) -> (u32, usize) {
        let (node, height) = if hi - lo == 1 {
            (Node::Leaf { key: lo as u32, depth: depth as u8 }, 0)
        } else {
            let (a, b) = (self.keys[lo], self.keys[hi - 1]);
            let end = (a.len as usize).min(b.len as usize);
            let shared = (depth..end).take_while(|&d| a.nib[d] == b.nib[d]).count();
            if shared > 0 {
                let (child, h) = self.build(lo, hi, depth + shared);
                let node =
                    Node::Ext { key: lo as u32, depth: depth as u8, len: shared as u8, child };
                (node, h + 1)
            } else {
                let mut children = [NONE; 16];
                let mut height = 0;
                let mut at = lo;
                while at < hi {
                    let nib = self.keys[at].nib[depth];
                    let run = self.keys[at..hi].iter().take_while(|k| k.nib[depth] == nib).count();
                    let (child, h) = self.build(at, at + run, depth + 1);
                    children[nib as usize] = child;
                    height = height.max(h + 1);
                    at += run;
                }
                (Node::Branch { children }, height)
            }
        };
        let id = self.nodes.len() as u32;
        self.nodes.push(node);
        self.by_height[height].push(id);
        (id, height)
    }

    /// Appends node `id`'s encoding to `enc`; its children are already referenced.
    fn encode<'a>(&mut self, id: u32, value: &impl Fn(usize) -> &'a [u8]) {
        let enc = &mut self.enc;
        match self.nodes[id as usize] {
            Node::Leaf { key, depth } => {
                let key = &self.keys[key as usize];
                let path = key.rest(depth as usize);
                let value = value(key.index as usize);
                put_list_header(enc, compact_len(path) + bytes_len(value));
                put_compact(enc, path, true);
                put_bytes(enc, value);
            }
            Node::Ext { key, depth, len, child } => {
                let key = &self.keys[key as usize];
                let path = &key.nib[depth as usize..(depth + len) as usize];
                let child = self.refs[child as usize];
                put_list_header(enc, compact_len(path) + ref_len(&child));
                put_compact(enc, path, false);
                put_ref(enc, &child);
            }
            Node::Branch { children } => {
                let refs = &self.refs;
                let child = |c: u32| (c != NONE).then(|| refs[c as usize]);
                let len: usize =
                    children.iter().map(|&c| child(c).map_or(1, |r| ref_len(&r))).sum();
                put_list_header(enc, len + 1);
                for &c in &children {
                    match child(c) {
                        Some(r) => put_ref(enc, &r),
                        None => enc.push(0x80),
                    }
                }
                enc.push(0x80);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ethrex_crypto::native::NativeCrypto;
    use ethrex_rlp::encode::RLPEncode;
    use ethrex_trie::Trie;

    use super::Lists;

    #[test]
    fn matches_the_trie_root() {
        let mut lists = Lists::default();
        let values: Vec<Vec<u8>> = (0..1100u32)
            .map(|i| (0..(i * 37 % 300) as u8).map(|b| b ^ i as u8).collect())
            .collect();
        for n in (0..300).chain([1000, 1100]) {
            let leaves = (0..n).map(|i| (i.encode_to_vec(), values[i].clone()));
            let want = Trie::compute_hash_from_unsorted_iter(leaves, &NativeCrypto);
            assert_eq!(lists.root(n, |i| &values[i]), want, "n={n}");
        }
    }
}
