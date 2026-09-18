use alloy_primitives::B256;
use helix_types::Payload;
use rustc_hash::FxHashSet;

#[derive(Default)]
pub(super) struct PayloadCache {
    cache: FxHashSet<B256>,
    slot: u64,
}

impl PayloadCache {
    pub fn insert(&mut self, payload: &Payload) -> bool {
        if payload.slot > self.slot {
            self.slot = payload.slot;
            self.cache.clear();
        }
        self.cache.insert(payload.execution_payload.execution_payload.block_hash)
    }
}
