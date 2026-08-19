//! Caches EVM execution state after a builder submission's transaction prefix so that a later
//! submission sharing the same parent, coinbase, and leading transactions can resume execution
//! from that point instead of re-running the whole block.
//!
//! Only two checkpoints are kept per `(parent_hash, coinbase)`: the state after all-but-the-last
//! transaction, and the state after all transactions. This covers the two dominant resubmission
//! patterns (the last transaction is replaced, or a transaction is appended) at O(1) extra
//! `State` clones per submission, rather than snapshotting at every transaction boundary.

use std::sync::RwLock;

use alloy_primitives::{Address, B256};
use dashmap::DashMap;
use reth_ethereum::{Receipt, primitives::BlockHeader};
use revm::database::{CacheState, TransitionState};

pub(crate) type PrefixCacheKey = (B256, Address);

/// Execution-relevant header fields that must match between the checkpoint's block and a new
/// submission before a cached checkpoint may be reused. Defense in depth: parent hash and
/// coinbase are already part of the cache key, but a builder could in principle vary other
/// fields (gas limit within its elastic bounds, for instance) between submissions.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HeaderFingerprint {
    gas_limit: u64,
    timestamp: u64,
    base_fee_per_gas: Option<u64>,
    prevrandao: Option<B256>,
    parent_beacon_block_root: Option<B256>,
    withdrawals_root: Option<B256>,
    excess_blob_gas: Option<u64>,
}

impl HeaderFingerprint {
    pub(crate) fn from_header(header: &impl BlockHeader) -> Self {
        Self {
            gas_limit: header.gas_limit(),
            timestamp: header.timestamp(),
            base_fee_per_gas: header.base_fee_per_gas(),
            prevrandao: header.mix_hash(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            withdrawals_root: header.withdrawals_root(),
            excess_blob_gas: header.excess_blob_gas(),
        }
    }
}

/// A snapshot of EVM execution state and receipt-builder accounting after executing some prefix
/// of a block's transactions.
pub(crate) struct Checkpoint {
    /// Hashes of the transactions already reflected in this checkpoint, in order.
    pub(crate) tx_hashes: Vec<B256>,
    pub(crate) apply_blacklist: bool,
    pub(crate) header_fingerprint: HeaderFingerprint,
    pub(crate) cache: CacheState,
    pub(crate) transition_state: Option<TransitionState>,
    pub(crate) receipts: Vec<Receipt>,
    pub(crate) cumulative_tx_gas_used: u64,
    pub(crate) block_regular_gas_used: u64,
    pub(crate) block_state_gas_used: u64,
    pub(crate) blob_gas_used: u64,
}

/// Raw pieces captured mid-execution, before they're paired with request-level metadata
/// (`apply_blacklist`, the header fingerprint) to become a [`Checkpoint`].
pub(crate) type ExecutionSnapshot =
    (Vec<B256>, CacheState, Option<TransitionState>, Vec<Receipt>, u64, u64, u64, u64);

impl Checkpoint {
    pub(crate) fn from_snapshot(
        snapshot: ExecutionSnapshot,
        apply_blacklist: bool,
        header_fingerprint: HeaderFingerprint,
    ) -> Self {
        let (
            tx_hashes,
            cache,
            transition_state,
            receipts,
            cumulative_tx_gas_used,
            block_regular_gas_used,
            block_state_gas_used,
            blob_gas_used,
        ) = snapshot;
        Self {
            tx_hashes,
            apply_blacklist,
            header_fingerprint,
            cache,
            transition_state,
            receipts,
            cumulative_tx_gas_used,
            block_regular_gas_used,
            block_state_gas_used,
            blob_gas_used,
        }
    }
}

impl Clone for Checkpoint {
    fn clone(&self) -> Self {
        Self {
            tx_hashes: self.tx_hashes.clone(),
            apply_blacklist: self.apply_blacklist,
            header_fingerprint: self.header_fingerprint.clone(),
            cache: self.cache.clone(),
            transition_state: self.transition_state.clone(),
            receipts: self.receipts.clone(),
            cumulative_tx_gas_used: self.cumulative_tx_gas_used,
            block_regular_gas_used: self.block_regular_gas_used,
            block_state_gas_used: self.block_state_gas_used,
            blob_gas_used: self.blob_gas_used,
        }
    }
}

/// The two checkpoints retained for a given `(parent_hash, coinbase)`, taken from the most
/// recently executed submission for that key.
pub(crate) struct CachedEntry {
    /// State after all transactions but the last.
    pub(crate) before_last: Checkpoint,
    /// State after all transactions.
    pub(crate) full: Checkpoint,
}

/// Concurrent, per-head cache of [`CachedEntry`]s keyed by `(parent_hash, coinbase)`.
///
/// Entries are wholesale-evicted whenever a submission arrives for a new parent hash, bounding
/// memory to the builders active for the current head rather than accumulating across slots —
/// the same single-head eviction idiom `ValidationApiInner::cached_state` already uses.
pub(crate) struct PrefixCache {
    latest_parent: RwLock<B256>,
    entries: DashMap<PrefixCacheKey, CachedEntry>,
}

impl PrefixCache {
    pub(crate) fn new() -> Self {
        Self { latest_parent: RwLock::new(B256::ZERO), entries: DashMap::new() }
    }

    /// Evicts all entries if `parent_hash` differs from the last-observed one.
    pub(crate) fn observe_parent(&self, parent_hash: B256) {
        let stale = *self.latest_parent.read().unwrap() != parent_hash;
        if stale {
            let mut latest = self.latest_parent.write().unwrap();
            if *latest != parent_hash {
                self.entries.clear();
                *latest = parent_hash;
            }
        }
    }

    /// Returns the longest usable checkpoint for `tx_hashes`, if any: a checkpoint is usable only
    /// if `tx_hashes` extends it exactly (same transactions, in order) and the blacklist flag and
    /// execution-relevant header fields match.
    pub(crate) fn find_match(
        &self,
        key: &PrefixCacheKey,
        tx_hashes: &[B256],
        apply_blacklist: bool,
        fingerprint: &HeaderFingerprint,
    ) -> Option<Checkpoint> {
        let entry = self.entries.get(key)?;

        let is_match = |cp: &Checkpoint| {
            cp.apply_blacklist == apply_blacklist &&
                cp.header_fingerprint == *fingerprint &&
                tx_hashes.len() >= cp.tx_hashes.len() &&
                tx_hashes[..cp.tx_hashes.len()] == cp.tx_hashes[..]
        };

        if is_match(&entry.full) {
            Some(entry.full.clone())
        } else if is_match(&entry.before_last) {
            Some(entry.before_last.clone())
        } else {
            None
        }
    }

    pub(crate) fn store(&self, key: PrefixCacheKey, entry: CachedEntry) {
        self.entries.insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    fn fp() -> HeaderFingerprint {
        HeaderFingerprint {
            gas_limit: 30_000_000,
            timestamp: 1,
            base_fee_per_gas: Some(7),
            prevrandao: None,
            parent_beacon_block_root: None,
            withdrawals_root: None,
            excess_blob_gas: None,
        }
    }

    fn checkpoint(tx_hashes: Vec<B256>, apply_blacklist: bool, fingerprint: HeaderFingerprint) -> Checkpoint {
        Checkpoint::from_snapshot(
            (tx_hashes, CacheState::default(), None, Vec::new(), 0, 0, 0, 0),
            apply_blacklist,
            fingerprint,
        )
    }

    const COINBASE: Address = address!("0x0000000000000000000000000000000000000001");

    #[test]
    fn matches_exact_append() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);
        let h2 = B256::repeat_byte(2);
        let h3 = B256::repeat_byte(3);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![h1], false, fp()),
                full: checkpoint(vec![h1, h2], false, fp()),
            },
        );

        // new submission appends h3 after the old full prefix [h1, h2]
        let found = cache.find_match(&key, &[h1, h2, h3], false, &fp());
        assert_eq!(found.unwrap().tx_hashes, vec![h1, h2]);
    }

    #[test]
    fn matches_last_tx_replaced() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);
        let h2 = B256::repeat_byte(2);
        let h2_new = B256::repeat_byte(9);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![h1], false, fp()),
                full: checkpoint(vec![h1, h2], false, fp()),
            },
        );

        let found = cache.find_match(&key, &[h1, h2_new], false, &fp());
        assert_eq!(found.unwrap().tx_hashes, vec![h1]);
    }

    #[test]
    fn coinbase_mismatch_is_a_cache_miss() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let other_key = (B256::ZERO, address!("0x0000000000000000000000000000000000000002"));
        let h1 = B256::repeat_byte(1);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![], false, fp()),
                full: checkpoint(vec![h1], false, fp()),
            },
        );

        assert!(cache.find_match(&other_key, &[h1], false, &fp()).is_none());
    }

    #[test]
    fn blacklist_flag_mismatch_is_a_cache_miss() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![], false, fp()),
                full: checkpoint(vec![h1], false, fp()),
            },
        );

        assert!(cache.find_match(&key, &[h1], true, &fp()).is_none());
    }

    #[test]
    fn header_fingerprint_mismatch_is_a_cache_miss() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![], false, fp()),
                full: checkpoint(vec![h1], false, fp()),
            },
        );

        let mut other = fp();
        other.gas_limit += 1;
        assert!(cache.find_match(&key, &[h1], false, &other).is_none());
    }

    #[test]
    fn parent_hash_change_evicts_all_entries() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);

        cache.observe_parent(B256::ZERO);
        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![], false, fp()),
                full: checkpoint(vec![h1], false, fp()),
            },
        );
        assert!(cache.find_match(&key, &[h1], false, &fp()).is_some());

        cache.observe_parent(B256::repeat_byte(0xAA));
        assert!(cache.find_match(&key, &[h1], false, &fp()).is_none());
    }

    #[test]
    fn divergent_prefix_is_a_cache_miss() {
        let cache = PrefixCache::new();
        let key = (B256::ZERO, COINBASE);
        let h1 = B256::repeat_byte(1);
        let h2 = B256::repeat_byte(2);
        let h_other = B256::repeat_byte(0xFF);

        cache.store(
            key,
            CachedEntry {
                before_last: checkpoint(vec![h1], false, fp()),
                full: checkpoint(vec![h1, h2], false, fp()),
            },
        );

        // first tx itself differs from what's cached -> no usable prefix at all
        assert!(cache.find_match(&key, &[h_other, h2], false, &fp()).is_none());
    }
}
