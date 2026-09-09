use std::{hash::Hash, net::IpAddr, sync::Arc};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

#[derive(Clone)]
pub struct IpTracker {
    tracker: Arc<RwLock<Tracker<32, IpAddr>>>,
}

impl IpTracker {
    pub fn increment(&self, slot: u64, addr: &IpAddr) -> f64 {
        self.tracker.write().increment(slot, addr)
    }
}

impl Default for IpTracker {
    fn default() -> Self {
        Self { tracker: Arc::new(RwLock::new(Tracker::default())) }
    }
}

/// Maintains per-slot counts in a rotating wheel. 
pub(crate) struct Tracker<const N: usize, K: Clone + Eq + Hash> {
    buckets: [FxHashMap<K, usize>; N],
    current: usize,
    seq: u64,
}

impl<const N: usize, K: Clone + Eq + Hash> Default for Tracker<N, K> {
    fn default() -> Self {
        Self {
           buckets: std::array::from_fn(|_| FxHashMap::default()),
           current: 0,
           seq: 0,
        }
    }
}

impl<const N: usize, K: Clone + Eq + Hash> Tracker<N, K> {
    pub fn frequency(&self, key: &K) -> f64 {
        let count: usize = self.buckets.iter().filter_map(|m| m.get(key)).sum();
        count as f64 / N as f64
    }

    pub fn increment(&mut self, seq: u64, key: &K) -> f64 {
        if seq > self.seq {
            self.seq = seq;
            self.current = (self.current + 1) % N;
            self.buckets[self.current].clear();
        }
        if seq == self.seq {
            self.buckets[self.current].entry(key.clone()).and_modify(|c| *c += 1).or_insert(1);
        }
        self.frequency(key)
    }
}