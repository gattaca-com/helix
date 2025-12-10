use alloy_primitives::B256;
use helix_common::api::proposer_api::GetHeaderParams;
use tokio::sync::oneshot;
use tracing::{error, warn};

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::{bid_adjustor::BidAdjustor, context::Context, types::GetHeaderResult},
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_get_header(
        &mut self,
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        assert_eq!(params.slot, self.bid_slot.as_u64(), "params should already be validated!");
        let _ = res_tx.send(self.get_header(params.parent_hash));
    }

    fn get_header(&mut self, parent_hash: B256) -> GetHeaderResult {
        let Some(best_block_hash) = self.bid_sorter.get_header(&parent_hash) else {
            warn!(%parent_hash, "no bids for this fork");
            return Err(ProposerApiError::NoBidPrepared);
        };

        let Some(original_bid) = self.payloads.get(&best_block_hash) else {
            error!("failed to get payload from bid sorter best, this should never happen!");
            return Err(ProposerApiError::NoBidPrepared);
        };

        if let Some(merged_bid) = self.block_merger.get_header(original_bid) {
            return Ok(merged_bid);
        };

        if let Some(adjusted_bid) = self.bid_adjustor.try_apply_adjustments(&original_bid) {
            let block_hash = adjusted_bid.block_hash();
            self.payloads.insert(*block_hash, adjusted_bid.clone().into());

            return Ok(adjusted_bid);
        }

        Ok(original_bid.clone())
    }
}
