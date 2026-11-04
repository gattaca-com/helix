use ethrex_common::{H256, constants::EMPTY_TRIE_HASH};

use crate::merge::rlp::{HashBatch, NodeRef, put_branch, put_ext, put_leaf};

const NO_NODE: u32 = u32::MAX;
// rlp(index) of a u32 is at most 5 bytes.
const NIBBLES: usize = 10;
const HEIGHTS: usize = NIBBLES + 1;

#[derive(Clone, Copy)]
struct IndexKey {
    nib: [u8; NIBBLES],
    len: u8,
    index: u32,
}

impl IndexKey {
    fn new(index: u32) -> IndexKey {
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
        IndexKey { nib, len: 2 * len as u8, index }
    }

    fn rest(&self, depth: usize) -> &[u8] {
        &self.nib[depth..self.len as usize]
    }
}

#[derive(Clone, Copy)]
enum ListNode {
    Leaf { key: u32, depth: u8 },
    Ext { key: u32, depth: u8, len: u8, child: u32 },
    Branch { children: [u32; 16] },
}

#[derive(Default)]
pub struct ListRoot {
    keys: Vec<IndexKey>,
    nodes: Vec<ListNode>,
    refs: Vec<NodeRef>,
    by_height: [Vec<u32>; HEIGHTS],
    batch: HashBatch,
}

impl ListRoot {
    pub fn compute<'a>(&mut self, n: usize, value: impl Fn(usize) -> &'a [u8]) -> H256 {
        if n == 0 {
            return *EMPTY_TRIE_HASH;
        }
        self.keys.clear();
        self.keys.extend((0..n as u32).map(IndexKey::new));
        self.keys.sort_unstable_by(|a, b| a.nib[..a.len as usize].cmp(&b.nib[..b.len as usize]));
        self.nodes.clear();
        for ids in &mut self.by_height {
            ids.clear();
        }
        let (root, _) = self.build(0, n, 0);
        self.refs.clear();
        self.refs.resize(self.nodes.len(), NodeRef::default());
        let mut batch = std::mem::take(&mut self.batch);
        for h in 0..HEIGHTS {
            batch.clear();
            for i in 0..self.by_height[h].len() {
                let id = self.by_height[h][i];
                let at = batch.enc.len();
                self.encode(id, &value, &mut batch.enc);
                if let Some(r) = batch.push(id, at) {
                    self.refs[id as usize] = r;
                }
            }
            for (id, r) in batch.hash() {
                self.refs[id as usize] = r;
            }
        }
        self.batch = batch;
        self.refs[root as usize].root()
    }

    fn build(&mut self, lo: usize, hi: usize, depth: usize) -> (u32, usize) {
        let (node, height) = if hi - lo == 1 {
            (ListNode::Leaf { key: lo as u32, depth: depth as u8 }, 0)
        } else {
            let (a, b) = (self.keys[lo], self.keys[hi - 1]);
            let end = (a.len as usize).min(b.len as usize);
            let shared = (depth..end).take_while(|&d| a.nib[d] == b.nib[d]).count();
            if shared > 0 {
                let (child, h) = self.build(lo, hi, depth + shared);
                let node =
                    ListNode::Ext { key: lo as u32, depth: depth as u8, len: shared as u8, child };
                (node, h + 1)
            } else {
                let mut children = [NO_NODE; 16];
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
                (ListNode::Branch { children }, height)
            }
        };
        let id = self.nodes.len() as u32;
        self.nodes.push(node);
        self.by_height[height].push(id);
        (id, height)
    }

    fn encode<'a>(&self, id: u32, value: &impl Fn(usize) -> &'a [u8], enc: &mut Vec<u8>) {
        match self.nodes[id as usize] {
            ListNode::Leaf { key, depth } => {
                let key = &self.keys[key as usize];
                put_leaf(enc, key.rest(depth as usize), value(key.index as usize));
            }
            ListNode::Ext { key, depth, len, child } => {
                let path = &self.keys[key as usize].nib[depth as usize..(depth + len) as usize];
                put_ext(enc, path, &self.refs[child as usize]);
            }
            ListNode::Branch { children } => {
                let child = children.map(|c| (c != NO_NODE).then(|| self.refs[c as usize]));
                put_branch(enc, &child);
            }
        }
    }
}
