use tracing::error;

use crate::{
    Api,
    auctioneer::{context::Context, types::BestMergeablePayload},
};

impl<A: Api> Context<A> {
    /// If the current best bid allows merging, return its payload
    pub(super) fn get_best_mergeable_payload(&self) -> BestMergeablePayload {
        let best = self.bid_sorter.best_mergeable()?;
        let entry = self.payloads.get(&best)?;

        let Some(data) = entry.to_header_data() else {
            error!("failed to merge data from payload entry, this should never happen!");
            return None;
        };

        Some(data)
    }
}
