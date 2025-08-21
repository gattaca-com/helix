use std::sync::Arc;

use alloy_primitives::B256;
use helix_types::BuilderBid;
use parking_lot::RwLock;

const MS_TO_STALE: u64 = 250;

struct BestMergedBlockEntry {
    /// Slot of the merged block
    slot: u64,
    /// Timestamp when the base block was fetched
    base_block_time_ns: u64,
    /// Hash of the parent of the merged block
    parent_block_hash: B256,
    /// The merged block bid
    bid: BuilderBid,
}

#[derive(Clone)]
pub struct BestMergedBlock(Arc<RwLock<Option<BestMergedBlockEntry>>>);

impl Default for BestMergedBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergedBlock {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    pub fn store(
        &self,
        slot: u64,
        base_block_time_ns: u64,
        parent_block_hash: B256,
        bid: BuilderBid,
    ) {
        let mut guard = self.0.write();
        *guard = Some(BestMergedBlockEntry { slot, base_block_time_ns, parent_block_hash, bid });
    }

    /// Loads the best merged block for the given slot and parent block hash.
    /// Returns a timestamp for when the base block was fetched, and the bid of the merged block.
    pub fn load(&self, slot: u64, parent_block_hash: &B256) -> Option<(u64, BuilderBid)> {
        self.0
            .read()
            .as_ref()
            .filter(|entry| entry.slot == slot && entry.parent_block_hash == *parent_block_hash)
            .map(|entry| (entry.base_block_time_ns, entry.bid.clone()))
    }
}
