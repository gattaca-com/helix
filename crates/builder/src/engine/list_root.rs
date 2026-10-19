use ethrex_common::{H256, constants::EMPTY_TRIE_HASH};
use ethrex_crypto::keccak::keccak_hash;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy)]
struct Key {
    len: usize,
    nib: [u8; 6],
}

impl Key {
    fn of(i: usize) -> Self {
        let (bytes, len): ([u8; 3], usize) = match i {
            0 => ([0x80, 0, 0], 1),
            1..=127 => ([i as u8, 0, 0], 1),
            128..=255 => ([0x81, i as u8, 0], 2),
            _ => ([0x82, (i >> 8) as u8, i as u8], 3),
        };
        let mut nib = [0u8; 6];
        for (b, byte) in bytes.iter().take(len).enumerate() {
            nib[2 * b] = byte >> 4;
            nib[2 * b + 1] = byte & 0x0f;
        }
        Key { len: 2 * len, nib }
    }
}

#[derive(Clone)]
enum Ref {
    Inline(Vec<u8>),
    Hash([u8; 32]),
}

#[derive(Default)]
pub struct ListRoot {
    memo: FxHashMap<(usize, u32, usize), Ref>,
    order: Vec<(Key, usize)>,
    leaves: FxHashMap<(usize, usize), Ref>,
}

impl ListRoot {
    pub fn root<'a>(&mut self, n: usize, stable: usize, value: impl Fn(usize) -> &'a [u8]) -> H256 {
        if n == 0 {
            return *EMPTY_TRIE_HASH;
        }
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        let ixs = (1..n.min(128)).chain(std::iter::once(0)).chain(128..n);
        order.extend(ixs.map(|i| (Key::of(i), i)));
        let mut todo = Vec::new();
        self.collect(&order, 0, 0, stable, &mut todo);
        let encoded: Vec<Vec<u8>> = todo
            .iter()
            .map(|&(key, i, depth)| {
                let mut payload = Vec::new();
                put_path(&mut payload, &key.nib[depth..key.len], true);
                put_bytes(&mut payload, value(i));
                let mut encoded = Vec::with_capacity(payload.len() + 9);
                put_header(&mut encoded, 0xc0, payload.len());
                encoded.extend_from_slice(&payload);
                encoded
            })
            .collect();
        let long: Vec<&[u8]> =
            encoded.iter().filter(|e| e.len() >= 32).map(Vec::as_slice).collect();
        let mut hashes = Vec::new();
        super::keccak8::hash_many(&long, &mut hashes);
        let mut hashes = hashes.into_iter();
        self.leaves.clear();
        for (&(_, i, depth), encoded) in todo.iter().zip(encoded) {
            let r = match encoded.len() < 32 {
                true => Ref::Inline(encoded),
                false => Ref::Hash(hashes.next().unwrap_or_default()),
            };
            self.leaves.insert((i, depth), r);
        }
        let root = self.node(&order, 0, 0, stable, &value);
        self.order = order;
        match root {
            Ref::Hash(hash) => H256(hash),
            Ref::Inline(encoded) => H256(keccak_hash(&encoded)),
        }
    }

    fn collect(
        &self,
        items: &[(Key, usize)],
        depth: usize,
        prefix: u32,
        stable: usize,
        out: &mut Vec<(Key, usize, usize)>,
    ) {
        if items.iter().all(|(_, i)| *i < stable) &&
            self.memo.contains_key(&(depth, prefix, items.len()))
        {
            return;
        }
        if let [(key, i)] = items {
            out.push((*key, *i, depth));
            return;
        }
        let (first, last) = (&items[0].0, &items[items.len() - 1].0);
        let mut shared = 0;
        while first.nib[depth + shared] == last.nib[depth + shared] {
            shared += 1;
        }
        if shared > 0 {
            let below = first.nib[depth..depth + shared]
                .iter()
                .fold(prefix, |p, n| (p << 4) | u32::from(*n));
            self.collect(items, depth + shared, below, stable, out);
            return;
        }
        let mut rest = items;
        for nib in 0..16u8 {
            let run = rest.iter().take_while(|(k, _)| k.nib[depth] == nib).count();
            if run > 0 {
                self.collect(&rest[..run], depth + 1, (prefix << 4) | u32::from(nib), stable, out);
                rest = &rest[run..];
            }
        }
    }

    fn node<'a>(
        &mut self,
        items: &[(Key, usize)],
        depth: usize,
        prefix: u32,
        stable: usize,
        value: &impl Fn(usize) -> &'a [u8],
    ) -> Ref {
        let memo = (depth, prefix, items.len());
        let fixed = items.iter().all(|(_, i)| *i < stable);
        if fixed && let Some(r) = self.memo.get(&memo) {
            return r.clone();
        }
        let mut payload = Vec::new();
        if let [(key, i)] = items {
            if let Some(r) = self.leaves.remove(&(*i, depth)) {
                if fixed {
                    self.memo.insert(memo, r.clone());
                }
                return r;
            }
            put_path(&mut payload, &key.nib[depth..key.len], true);
            put_bytes(&mut payload, value(*i));
        } else {
            let (first, last) = (&items[0].0, &items[items.len() - 1].0);
            let mut shared = 0;
            while first.nib[depth + shared] == last.nib[depth + shared] {
                shared += 1;
            }
            if shared > 0 {
                let nibs = &first.nib[depth..depth + shared];
                let below = nibs.iter().fold(prefix, |p, n| (p << 4) | u32::from(*n));
                let child = self.node(items, depth + shared, below, stable, value);
                put_path(&mut payload, nibs, false);
                put_ref(&mut payload, &child);
            } else {
                let mut rest = items;
                for nib in 0..16u8 {
                    let run = rest.iter().take_while(|(k, _)| k.nib[depth] == nib).count();
                    if run == 0 {
                        payload.push(0x80);
                        continue;
                    }
                    let below = (prefix << 4) | u32::from(nib);
                    let child = self.node(&rest[..run], depth + 1, below, stable, value);
                    put_ref(&mut payload, &child);
                    rest = &rest[run..];
                }
                payload.push(0x80);
            }
        }
        let mut encoded = Vec::with_capacity(payload.len() + 9);
        put_header(&mut encoded, 0xc0, payload.len());
        encoded.extend_from_slice(&payload);
        let r = if encoded.len() < 32 {
            Ref::Inline(encoded)
        } else {
            Ref::Hash(keccak_hash(&encoded))
        };
        if fixed {
            self.memo.insert(memo, r.clone());
        }
        r
    }
}

fn put_header(out: &mut Vec<u8>, base: u8, len: usize) {
    if len < 56 {
        out.push(base + len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    out.push(base + 55 + (bytes.len() - skip) as u8);
    out.extend_from_slice(&bytes[skip..]);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if let [byte] = bytes &&
        *byte < 0x80
    {
        out.push(*byte);
        return;
    }
    put_header(out, 0x80, bytes.len());
    out.extend_from_slice(bytes);
}

fn put_path(out: &mut Vec<u8>, nibs: &[u8], leaf: bool) {
    let flag = if leaf { 2 } else { 0 } + (nibs.len() % 2) as u8;
    let mut compact = Vec::with_capacity(nibs.len() / 2 + 1);
    let rest = if nibs.len() % 2 == 1 {
        compact.push((flag << 4) | nibs[0]);
        &nibs[1..]
    } else {
        compact.push(flag << 4);
        nibs
    };
    compact.extend(rest.chunks(2).map(|pair| (pair[0] << 4) | pair[1]));
    put_bytes(out, &compact);
}

fn put_ref(out: &mut Vec<u8>, r: &Ref) {
    match r {
        Ref::Inline(encoded) => out.extend_from_slice(encoded),
        Ref::Hash(hash) => {
            out.push(0xa0);
            out.extend_from_slice(hash);
        }
    }
}
