use alloy_primitives::B256;
use helix_common::{api::proposer_api::GetHeaderParams, utils::utcnow_ms};
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::{PayloadHeaderData, context::Context, types::GetHeaderResult},
};

impl Context {
    pub(super) fn handle_get_header(
        &self,
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        assert_eq!(params.slot, self.bid_slot.as_u64(), "params should already be validated!");
        let _ = res_tx.send(self.get_header(params.parent_hash));
    }

    fn get_header(&self, parent_hash: B256) -> GetHeaderResult {
        let Some(best_block_hash) = self.bid_sorter.get_header(&parent_hash) else {
            warn!(%parent_hash, "no bids for this fork");
            return Err(ProposerApiError::NoBidPrepared);
        };

        let Some(entry) = self.payloads.get(&best_block_hash) else {
            error!("failed to get payload from bid sorter best, this should never happen!");
            return Err(ProposerApiError::NoBidPrepared);
        };

        let Some(original_bid) = entry.to_header_data() else {
            error!("failed to get header data from payload entry, this should never happen!");
            return Err(ProposerApiError::NoBidPrepared);
        };

        if self._config.block_merging_config.is_enabled {
            let Some((time, merged_bid)) = self.block_merger.get_header(&parent_hash) else {
                return Ok(original_bid);
            };

            let merging_preferences = &self._config.block_merging_config.max_merged_bid_age_ms;
            if merged_bid_higher(&merged_bid, &original_bid, time, *merging_preferences) {
                return Ok(merged_bid);
            }
        }

        Ok(original_bid)
    }
}

fn merged_bid_higher(
    merged_bid: &PayloadHeaderData,
    original_bid: &PayloadHeaderData,
    time: u64,
    max_merged_bid_age_ms: u64,
) -> bool {
    // If the current best bid has equal or higher value, we use that
    if merged_bid.value() <= original_bid.value() {
        debug!(
            "merged bid {:?} with value {:?} is not higher than regular bid, using regular bid, value = {:?}, block_hash = {:?}",
            merged_bid.value(),
            merged_bid.block_hash(),
            original_bid.value(),
            original_bid.block_hash()
        );
        return false;
    }
    // If the merged bid is stale, we use the current best bid
    let now_ms = utcnow_ms();
    if time < now_ms - max_merged_bid_age_ms {
        debug!(
            "merged bid {:?} with value {:?} is stale ({} ms old), using regular bid, value = {:?}, block_hash = {:?}",
            merged_bid.value(),
            merged_bid.block_hash(),
            now_ms - time,
            original_bid.value(),
            original_bid.block_hash()
        );
        return false;
    }

    debug!(
        "using merged bid, value = {:?}, block_hash = {:?}",
        merged_bid.value(),
        merged_bid.block_hash()
    );
    true
}
