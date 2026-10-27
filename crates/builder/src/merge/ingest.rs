//! Forwarded blocks in, the slot's pool out. Every tx is decoded and its sender recovered once
//! per slot into one table; orders and bases name txs by index into it. An appendable block comes
//! back as a base.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types::beacon::BlsPublicKey;
use ethrex_common::{
    H256,
    constants::DEFAULT_OMMERS_HASH,
    types::{BlockHeader, Transaction, Withdrawal},
};
use ethrex_crypto::native::NativeCrypto;
use helix_tcp_types::merging::{
    builder_to_relay::RejectCode,
    order::{
        MAX_BLOCK_TXS, MAX_BUNDLE_TXS, MAX_ORDERS_PER_BLOCK, MAX_TX_BYTES, MergeOrderRef,
        bundle_order_hash, is_tx_hash_ref, order_id,
    },
    relay_to_builder::MergeableBlockV1,
};
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::Decode;

use crate::convert::{eaddr, ewithdrawal, h256};

pub struct Tx {
    pub tx: Transaction,
    pub sender: ethrex_common::Address,
    pub hash: B256,
    /// Canonical encoding, as the wire carried it.
    pub encoded: Bytes,
}

impl Tx {
    pub fn blobs(&self) -> &[H256] {
        match &self.tx {
            Transaction::EIP4844Transaction(tx) => &tx.blob_versioned_hashes,
            Transaction::FrameTransaction(tx) => &tx.blob_versioned_hashes,
            _ => &[],
        }
    }
}

/// A run of items in a flat buffer: `Pool::refs`, or an `Effects` table.
#[derive(Clone, Copy, Default)]
pub struct Range {
    pub at: u32,
    pub len: u32,
}

impl Range {
    /// From `at` to the end of a buffer now `len` long.
    pub fn to(at: usize, len: usize) -> Range {
        Range { at: at as u32, len: (len - at) as u32 }
    }

    pub fn of<T>(self, items: &[T]) -> &[T] {
        &items[self.at as usize..][..self.len as usize]
    }

    /// The first `n` items and the rest.
    pub fn split(self, n: u32) -> (Range, Range) {
        debug_assert!(n <= self.len);
        (Range { at: self.at, len: n }, Range { at: self.at + n, len: self.len - n })
    }
}

/// A builder that forwarded blocks this slot.
pub struct Source {
    pub pubkey: BlsPublicKey,
    pub address: Address,
}

/// A mergeable order: up to `MAX_BUNDLE_TXS` txs, so per-tx flags fit a `u32`.
#[derive(Clone, Copy)]
pub struct Order {
    pub txs: Range,
    /// Bit `i`: tx `i` may revert.
    pub revert: u32,
    /// Bit `i`: tx `i` may be left out, but not revert.
    pub drop: u32,
    /// The highest-value block that carried it; that block's source gets the credit.
    pub block: u32,
    pub source: u16,
    /// A latest-only bundle its builder's newer submission left out. Final for the slot.
    pub withdrawn: bool,
}

/// An appendable block, kept as a merge base.
#[derive(Default)]
pub struct Base {
    pub hash: B256,
    pub recv_ns: u64,
    pub block: u32,
    pub source: u16,
    /// Fields as the payload carried them, `gas_used` included; `Merge::open` checks the derived
    /// ones against the parent and fills the rest.
    pub header: BlockHeader,
    pub withdrawals: Vec<Withdrawal>,
    pub txs: Range,
}

pub struct Reject {
    pub code: RejectCode,
    pub label: &'static str,
    pub block_hash: Option<B256>,
    pub msg: String,
}

#[derive(Default)]
pub struct Ingested {
    pub base: Option<Base>,
    pub reject: Option<Reject>,
}

/// One slot's input on one relay connection. Append-only, so every index stays valid for the
/// slot; an order seen again from a higher-value block is updated in place. `clear` keeps the
/// capacity for the next slot.
#[derive(Default)]
pub struct Pool {
    pub txs: Vec<Tx>,
    /// Resolves tx-hash references, which the relay scopes the same way (per connection, per
    /// slot).
    tx_ix: FxHashMap<B256, u32>,
    /// Tx indices: order txs and base bodies, each a `Range`.
    pub refs: Vec<u32>,
    pub orders: Vec<Order>,
    /// By order index.
    pub ids: Vec<B256>,
    order_ix: FxHashMap<B256, u32>,
    pub sources: Vec<Source>,
    pub blocks: FxHashSet<B256>,
    /// Block values, by block index.
    pub values: Vec<U256>,
    /// (source, order) for the latest-only bundles in each source's newest block.
    latest: Vec<(u16, u32)>,
    /// The block being ingested: its txs by its own index.
    block_txs: Vec<u32>,
    /// Bumped whenever an order is added or changed; merges re-extend only when it moves.
    pub version: u64,
}

impl Pool {
    pub fn tx(&self, ix: u32) -> &Tx {
        debug_assert!((ix as usize) < self.txs.len());
        &self.txs[ix as usize]
    }

    pub fn refs(&self, range: Range) -> &[u32] {
        range.of(&self.refs)
    }

    pub fn clear(&mut self) {
        self.txs.clear();
        self.tx_ix.clear();
        self.refs.clear();
        self.orders.clear();
        self.ids.clear();
        self.order_ix.clear();
        self.sources.clear();
        self.blocks.clear();
        self.values.clear();
        self.latest.clear();
        self.version = 0;
    }

    pub fn block(
        &mut self,
        body: &[u8],
        recv_ns: u64,
        slot: u64,
        parent_hash: B256,
        max_blocks: usize,
        max_orders: usize,
    ) -> Ingested {
        let reject = |code, label, block_hash, msg: String| Ingested {
            base: None,
            reject: Some(Reject { code, label, block_hash, msg }),
        };
        let msg = match MergeableBlockV1::from_ssz_bytes(body) {
            Ok(msg) => msg,
            Err(e) => {
                let msg = format!("undecodable block: {e:?}");
                return reject(RejectCode::InvalidOrder, "undecodable", None, msg);
            }
        };
        let v1 = &msg.execution_payload.payload_inner.payload_inner;
        let hash = Some(v1.block_hash);
        if msg.slot != slot {
            return reject(RejectCode::StaleSlot, "stale_slot", hash, "stale slot".into());
        }
        if self.blocks.contains(&v1.block_hash) {
            return Ingested::default();
        }
        // Also bounds `sources`, which is indexed by `u16`.
        if self.blocks.len() >= max_blocks.min(u16::MAX as usize) {
            let msg = "max blocks per slot".into();
            return reject(RejectCode::LimitExceeded, "max_blocks", hash, msg);
        }
        if v1.transactions.len() > MAX_BLOCK_TXS {
            let msg = "max block txs".into();
            return reject(RejectCode::LimitExceeded, "max_block_txs", hash, msg);
        }

        // The relay sends each tx whole once per slot, so a block's txs reach the table before
        // any other check rejects it, or each later block that references them fails too.
        self.block_txs.clear();
        let mut failed = None;
        let mut oversized = false;
        for bytes in &v1.transactions {
            if bytes.len() > MAX_TX_BYTES {
                oversized = true;
                continue;
            }
            match self.resolve(bytes) {
                Ok(ix) => self.block_txs.push(ix),
                Err(err) => {
                    failed.get_or_insert(err);
                }
            }
        }
        if v1.parent_hash != parent_hash {
            let msg = "base block parent does not match the builder's head".into();
            return reject(RejectCode::HeadMismatch, "head_mismatch", hash, msg);
        }
        if oversized {
            let msg = "oversized transaction".into();
            return reject(RejectCode::InvalidOrder, "oversized_tx", hash, msg);
        }
        if msg.merge_orders.len() > MAX_ORDERS_PER_BLOCK {
            let msg = "max orders per block".into();
            return reject(RejectCode::LimitExceeded, "max_block_orders", hash, msg);
        }
        if let Some(err) = failed {
            return reject(RejectCode::InvalidOrder, "invalid_tx", hash, err);
        }
        if msg.merge_orders.iter().any(|order| order.validate(self.block_txs.len()).is_err()) {
            let msg = "order ref out of range".into();
            return reject(RejectCode::InvalidOrder, "order_ref", hash, msg);
        }

        let key = |s: &Source| s.pubkey == msg.builder_pubkey && s.address == msg.builder_address;
        let source = match self.sources.iter().position(key) {
            Some(i) => i as u16,
            None => {
                let source = Source { pubkey: msg.builder_pubkey, address: msg.builder_address };
                self.sources.push(source);
                (self.sources.len() - 1) as u16
            }
        };
        let block = self.values.len() as u32;
        self.values.push(msg.block_value);
        self.blocks.insert(v1.block_hash);
        let full = self.add(&msg, block, source, max_orders);

        let base = msg.allow_appending.then(|| {
            let txs = Range { at: self.refs.len() as u32, len: self.block_txs.len() as u32 };
            self.refs.extend_from_slice(&self.block_txs);
            let header = BlockHeader {
                parent_hash: h256(v1.parent_hash),
                ommers_hash: *DEFAULT_OMMERS_HASH,
                coinbase: eaddr(v1.fee_recipient),
                number: v1.block_number,
                gas_limit: v1.gas_limit,
                gas_used: v1.gas_used,
                timestamp: v1.timestamp,
                extra_data: v1.extra_data.0.clone(),
                prev_randao: h256(v1.prev_randao),
                base_fee_per_gas: u64::try_from(v1.base_fee_per_gas).ok(),
                excess_blob_gas: Some(msg.execution_payload.excess_blob_gas),
                ..Default::default()
            };
            let withdrawals = &msg.execution_payload.payload_inner.withdrawals;
            Base {
                hash: v1.block_hash,
                recv_ns,
                block,
                source,
                header,
                withdrawals: withdrawals.iter().map(ewithdrawal).collect(),
                txs,
            }
        });
        Ingested {
            // Orders dropped for the pool cap still leave a usable base.
            reject: full.then(|| Reject {
                code: RejectCode::LimitExceeded,
                label: "max_orders",
                block_hash: hash,
                msg: "max orders per slot".into(),
            }),
            base,
        }
    }

    /// A raw tx, or a reference to one already seen whole, decoded once per slot.
    fn resolve(&mut self, bytes: &Bytes) -> Result<u32, String> {
        if is_tx_hash_ref(bytes) {
            let hash = B256::from_slice(bytes);
            return self
                .tx_ix
                .get(&hash)
                .copied()
                .ok_or_else(|| format!("unresolved tx hash reference: {hash}"));
        }
        let hash = keccak256(bytes);
        if let Some(&ix) = self.tx_ix.get(&hash) {
            return Ok(ix);
        }
        let tx = Transaction::decode_canonical(bytes).map_err(|e| format!("tx decode: {e}"))?;
        let sender = tx.sender(&NativeCrypto).map_err(|e| format!("sender recovery: {e}"))?;
        let ix = self.txs.len() as u32;
        self.txs.push(Tx { tx, sender, hash, encoded: bytes.clone() });
        self.tx_ix.insert(hash, ix);
        Ok(ix)
    }

    /// Pools `msg`'s orders; true when some were dropped for the `max` cap.
    fn add(&mut self, msg: &MergeableBlockV1, block: u32, source: u16, max: usize) -> bool {
        let previous = self.latest.len();
        let mut changed = false;
        let mut full = false;
        for order_ref in &msg.merge_orders {
            let mut hashes = [B256::ZERO; MAX_BUNDLE_TXS];
            let bits = |ixs: &[u16]| ixs.iter().fold(0u32, |bits, &i| bits | 1 << i);
            let (hash, revert, drop, latest_only) = match order_ref {
                MergeOrderRef::Tx(r) => {
                    let tx = self.block_txs[r.index as usize];
                    (self.txs[tx as usize].hash, u32::from(r.can_revert), 0, false)
                }
                MergeOrderRef::Bundle(bundle) => {
                    for (hash, &i) in hashes.iter_mut().zip(&bundle.txs) {
                        *hash = self.txs[self.block_txs[i as usize] as usize].hash;
                    }
                    let hash = bundle_order_hash(&hashes[..bundle.txs.len()]);
                    let (revert, drop) = (bits(&bundle.reverting_txs), bits(&bundle.dropping_txs));
                    (hash, revert, drop, bundle.latest_only)
                }
            };
            let id = order_id(hash, &msg.builder_pubkey);
            let ix = match self.order_ix.get(&id) {
                // The same id is the same txs; only the attribution moves, to the highest-value
                // block.
                Some(&ix) => {
                    let order = &mut self.orders[ix as usize];
                    if msg.block_value > self.values[order.block as usize] {
                        order.revert = revert;
                        order.drop = drop;
                        order.block = block;
                        order.source = source;
                        changed = true;
                    }
                    ix
                }
                None if self.orders.len() >= max => {
                    full = true;
                    continue;
                }
                None => {
                    let at = self.refs.len() as u32;
                    match order_ref {
                        MergeOrderRef::Tx(r) => self.refs.push(self.block_txs[r.index as usize]),
                        MergeOrderRef::Bundle(bundle) => {
                            for &i in &bundle.txs {
                                self.refs.push(self.block_txs[i as usize]);
                            }
                        }
                    }
                    let txs = Range { at, len: self.refs.len() as u32 - at };
                    let ix = self.orders.len() as u32;
                    let withdrawn = false;
                    self.orders.push(Order { txs, revert, drop, block, source, withdrawn });
                    self.ids.push(id);
                    self.order_ix.insert(id, ix);
                    changed = true;
                    ix
                }
            };
            if latest_only {
                self.latest.push((source, ix));
            }
        }

        // A latest-only bundle missing from the builder's newest submission is withdrawn.
        for i in 0..previous {
            let (s, ix) = self.latest[i];
            if s == source && !self.latest[previous..].contains(&(s, ix)) {
                changed |= !self.orders[ix as usize].withdrawn;
                self.orders[ix as usize].withdrawn = true;
            }
        }
        let mut i = 0;
        self.latest.retain(|&(s, _)| {
            i += 1;
            i > previous || s != source
        });
        self.version += u64::from(changed);
        full
    }
}
