use std::sync::Arc;

use alloy_primitives::{B256, U256};
use helix_types::BuilderBid;
use parking_lot::RwLock;

#[derive(Clone)]
pub struct BestMergedBlock(Arc<RwLock<Option<(u64, B256, BuilderBid)>>>);

impl Default for BestMergedBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergedBlock {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    pub fn store(&self, slot: u64, parent_block_hash: B256, bid: BuilderBid) {
        let mut guard = self.0.write();
        if bid.value() >= guard.as_ref().map(|(_, _, b)| b.value()).unwrap_or(&U256::ZERO) {
            *guard = Some((slot, parent_block_hash, bid));
        }
    }

    pub fn load(&self, slot: u64, parent_block_hash: &B256) -> Option<BuilderBid> {
        self.0
            .read()
            .as_ref()
            .filter(|(s, p, _)| *s == slot && *p == *parent_block_hash)
            .map(|(_, _, bid)| bid.clone())
    }
}
