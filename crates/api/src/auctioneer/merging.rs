use crate::{
    auctioneer::{context::Context, types::BestMergeablePayload},
    Api,
};

impl<A: Api> Context<A> {
    /// If the current best bid allows merging, return its payload
    pub(super) fn get_best_mergeable_payload(&self) -> BestMergeablePayload {
        let best = self.bid_sorter.best_mergeable()?;
        let payload = self.payloads.get(&best.header.block_hash)?;
        Some((best, payload.clone()))
    }
}
