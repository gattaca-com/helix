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
    pub encoded: Bytes,
}

impl Tx {
    pub fn blob_hashes(&self) -> &[H256] {
        match &self.tx {
            Transaction::EIP4844Transaction(tx) => &tx.blob_versioned_hashes,
            Transaction::FrameTransaction(tx) => &tx.blob_versioned_hashes,
            _ => &[],
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Range {
    pub at: u32,
    pub len: u32,
}

impl Range {
    pub fn between(at: usize, len: usize) -> Range {
        Range { at: at as u32, len: (len - at) as u32 }
    }

    pub fn slice<T>(self, items: &[T]) -> &[T] {
        &items[self.at as usize..][..self.len as usize]
    }

    pub fn split_at(self, n: u32) -> (Range, Range) {
        debug_assert!(n <= self.len);
        (Range { at: self.at, len: n }, Range { at: self.at + n, len: self.len - n })
    }
}

pub struct Submitter {
    pub pubkey: BlsPublicKey,
    pub address: Address,
}

#[derive(Clone, Copy)]
pub struct Order {
    pub txs: Range,
    pub may_revert: u32,
    pub may_drop: u32,
    pub best_block: u32,
    pub submitter: u16,
    pub withdrawn: bool,
}

#[derive(Default)]
pub struct BaseBlock {
    pub hash: B256,
    pub recv_ns: u64,
    pub block: u32,
    pub submitter: u16,
    pub header: BlockHeader,
    pub withdrawals: Vec<Withdrawal>,
    pub txs: Range,
}

impl BaseBlock {
    pub fn copy_from(&mut self, other: &BaseBlock) {
        self.hash = other.hash;
        self.recv_ns = other.recv_ns;
        self.block = other.block;
        self.submitter = other.submitter;
        self.header.clone_from(&other.header);
        self.withdrawals.clone_from(&other.withdrawals);
        self.txs = other.txs;
    }
}

pub struct Reject {
    pub code: RejectCode,
    pub label: &'static str,
    pub block_hash: Option<B256>,
    pub msg: String,
}

#[derive(Default)]
pub struct Added {
    pub base: Option<BaseBlock>,
    pub orders_dropped: Option<Reject>,
}

#[derive(Default)]
pub struct Pool {
    pub txs: Vec<Tx>,
    // (gas limit, blob count) per tx, split from `txs` for the per-order fit checks.
    tx_fit: Vec<(u64, u64)>,
    tx_by_hash: FxHashMap<B256, u32>,
    pub tx_refs: Vec<u32>,
    pub orders: Vec<Order>,
    pub order_ids: Vec<B256>,
    order_by_id: FxHashMap<B256, u32>,
    pub submitters: Vec<Submitter>,
    pub block_hashes: FxHashSet<B256>,
    pub block_values: Vec<U256>,
    latest_bundles: Vec<(u16, u32)>,
    submission_txs: Vec<u32>,
    pub orders_version: u64,
}

impl Pool {
    pub fn tx(&self, ix: u32) -> &Tx {
        debug_assert!((ix as usize) < self.txs.len());
        &self.txs[ix as usize]
    }

    pub fn tx_fit(&self, ix: u32) -> (u64, u64) {
        debug_assert!((ix as usize) < self.tx_fit.len());
        self.tx_fit[ix as usize]
    }

    pub fn txs_of(&self, range: Range) -> &[u32] {
        range.slice(&self.tx_refs)
    }

    pub fn clear(&mut self) {
        self.txs.clear();
        self.tx_fit.clear();
        self.tx_by_hash.clear();
        self.tx_refs.clear();
        self.orders.clear();
        self.order_ids.clear();
        self.order_by_id.clear();
        self.submitters.clear();
        self.block_hashes.clear();
        self.block_values.clear();
        self.latest_bundles.clear();
        self.orders_version = 0;
    }

    pub fn add(
        &mut self,
        body: &[u8],
        recv_ns: u64,
        slot: u64,
        parent_hash: B256,
        max_blocks: usize,
        max_orders: usize,
    ) -> Result<Added, Reject> {
        let reject =
            |code, label, block_hash, msg: String| Err(Reject { code, label, block_hash, msg });
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
        if self.block_hashes.contains(&v1.block_hash) {
            return Ok(Added::default());
        }
        // Also bounds `submitters`, indexed by u16.
        if self.block_hashes.len() >= max_blocks.min(u16::MAX as usize) {
            let msg = "max blocks per slot".into();
            return reject(RejectCode::LimitExceeded, "max_blocks", hash, msg);
        }
        if v1.transactions.len() > MAX_BLOCK_TXS {
            let msg = "max block txs".into();
            return reject(RejectCode::LimitExceeded, "max_block_txs", hash, msg);
        }

        // Txs enter the table before any reject, or every later block referencing them fails.
        self.submission_txs.clear();
        let mut failed = None;
        let mut oversized = false;
        for bytes in &v1.transactions {
            if bytes.len() > MAX_TX_BYTES {
                oversized = true;
                continue;
            }
            match self.insert_tx(bytes) {
                Ok(ix) => self.submission_txs.push(ix),
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
        if msg.merge_orders.iter().any(|order| order.validate(self.submission_txs.len()).is_err()) {
            let msg = "order ref out of range".into();
            return reject(RejectCode::InvalidOrder, "order_ref", hash, msg);
        }

        let key =
            |s: &Submitter| s.pubkey == msg.builder_pubkey && s.address == msg.builder_address;
        let submitter = match self.submitters.iter().position(key) {
            Some(i) => i as u16,
            None => {
                let submitter =
                    Submitter { pubkey: msg.builder_pubkey, address: msg.builder_address };
                self.submitters.push(submitter);
                (self.submitters.len() - 1) as u16
            }
        };
        let block = self.block_values.len() as u32;
        self.block_values.push(msg.block_value);
        self.block_hashes.insert(v1.block_hash);
        let full = self.add_orders(&msg, block, submitter, max_orders);

        let base = msg.allow_appending.then(|| {
            let txs =
                Range { at: self.tx_refs.len() as u32, len: self.submission_txs.len() as u32 };
            self.tx_refs.extend_from_slice(&self.submission_txs);
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
            BaseBlock {
                hash: v1.block_hash,
                recv_ns,
                block,
                submitter,
                header,
                withdrawals: withdrawals.iter().map(ewithdrawal).collect(),
                txs,
            }
        });
        let orders_dropped = full.then(|| Reject {
            code: RejectCode::LimitExceeded,
            label: "max_orders",
            block_hash: hash,
            msg: "max orders per slot".into(),
        });
        Ok(Added { base, orders_dropped })
    }

    fn insert_tx(&mut self, bytes: &Bytes) -> Result<u32, String> {
        if is_tx_hash_ref(bytes) {
            let hash = B256::from_slice(bytes);
            return self
                .tx_by_hash
                .get(&hash)
                .copied()
                .ok_or_else(|| format!("unresolved tx hash reference: {hash}"));
        }
        let hash = keccak256(bytes);
        if let Some(&ix) = self.tx_by_hash.get(&hash) {
            return Ok(ix);
        }
        let tx = Transaction::decode_canonical(bytes).map_err(|e| format!("tx decode: {e}"))?;
        let sender = tx.sender(&NativeCrypto).map_err(|e| format!("sender recovery: {e}"))?;
        let ix = self.txs.len() as u32;
        self.txs.push(Tx { tx, sender, hash, encoded: bytes.clone() });
        let tx = &self.txs[ix as usize];
        self.tx_fit.push((tx.tx.gas_limit(), tx.blob_hashes().len() as u64));
        self.tx_by_hash.insert(hash, ix);
        Ok(ix)
    }

    fn add_orders(
        &mut self,
        msg: &MergeableBlockV1,
        block: u32,
        submitter: u16,
        max: usize,
    ) -> bool {
        let previous = self.latest_bundles.len();
        let mut changed = false;
        let mut full = false;
        for order_ref in &msg.merge_orders {
            let mut hashes = [B256::ZERO; MAX_BUNDLE_TXS];
            let bits = |ixs: &[u16]| ixs.iter().fold(0u32, |bits, &i| bits | 1 << i);
            let (hash, revert, drop, latest_only) = match order_ref {
                MergeOrderRef::Tx(r) => {
                    let tx = self.submission_txs[r.index as usize];
                    (self.txs[tx as usize].hash, u32::from(r.can_revert), 0, false)
                }
                MergeOrderRef::Bundle(bundle) => {
                    for (hash, &i) in hashes.iter_mut().zip(&bundle.txs) {
                        *hash = self.txs[self.submission_txs[i as usize] as usize].hash;
                    }
                    let hash = bundle_order_hash(&hashes[..bundle.txs.len()]);
                    let (revert, drop) = (bits(&bundle.reverting_txs), bits(&bundle.dropping_txs));
                    (hash, revert, drop, bundle.latest_only)
                }
            };
            let id = order_id(hash, &msg.builder_pubkey);
            let ix = match self.order_by_id.get(&id) {
                Some(&ix) => {
                    let order = &mut self.orders[ix as usize];
                    if msg.block_value > self.block_values[order.best_block as usize] {
                        order.may_revert = revert;
                        order.may_drop = drop;
                        order.best_block = block;
                        order.submitter = submitter;
                        changed = true;
                    }
                    ix
                }
                None if self.orders.len() >= max => {
                    full = true;
                    continue;
                }
                None => {
                    let at = self.tx_refs.len() as u32;
                    match order_ref {
                        MergeOrderRef::Tx(r) => {
                            self.tx_refs.push(self.submission_txs[r.index as usize])
                        }
                        MergeOrderRef::Bundle(bundle) => {
                            for &i in &bundle.txs {
                                self.tx_refs.push(self.submission_txs[i as usize]);
                            }
                        }
                    }
                    let txs = Range { at, len: self.tx_refs.len() as u32 - at };
                    let ix = self.orders.len() as u32;
                    let withdrawn = false;
                    self.orders.push(Order {
                        txs,
                        may_revert: revert,
                        may_drop: drop,
                        best_block: block,
                        submitter,
                        withdrawn,
                    });
                    self.order_ids.push(id);
                    self.order_by_id.insert(id, ix);
                    changed = true;
                    ix
                }
            };
            if latest_only {
                self.latest_bundles.push((submitter, ix));
            }
        }

        for i in 0..previous {
            let (s, ix) = self.latest_bundles[i];
            if s == submitter && !self.latest_bundles[previous..].contains(&(s, ix)) {
                changed |= !self.orders[ix as usize].withdrawn;
                self.orders[ix as usize].withdrawn = true;
            }
        }
        let mut i = 0;
        self.latest_bundles.retain(|&(s, _)| {
            i += 1;
            i > previous || s != submitter
        });
        self.orders_version += u64::from(changed);
        full
    }
}
