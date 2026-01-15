use std::{sync::atomic::Ordering, time::Instant};

use alloy_primitives::B256;
use helix_common::{api::proposer_api::GetHeaderParams, metrics::BID_ADJUSTMENT_LATENCY};
use tokio::sync::oneshot;
use tracing::{error, warn};

use crate::{
    api::proposer::ProposerApiError,
    auctioneer::{
        bid_adjustor::BidAdjustor,
        context::Context,
        types::{GetHeaderResult, SlotData},
    },
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_get_header(
        &mut self,
        params: GetHeaderParams,
        slot_data: &SlotData,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        assert_eq!(params.slot, self.bid_slot.as_u64(), "params should already be validated!");
        let _ = res_tx.send(self.get_header(params.parent_hash, slot_data));
    }

    fn get_header(&mut self, parent_hash: B256, slot_data: &SlotData) -> GetHeaderResult {
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

        let original_bid = original_bid.clone();
        if self.adjustments_enabled.load(Ordering::Relaxed) {
            let start = Instant::now();
            if let Some((adjusted_bid, sim_request, is_adjustable_slot, strategy)) =
                self.bid_adjustor.try_apply_adjustments(&original_bid, slot_data, false)
            {
                BID_ADJUSTMENT_LATENCY
                    .with_label_values(&[strategy])
                    .observe(start.elapsed().as_micros() as f64);

                self.store_data_and_sim(sim_request, adjusted_bid.clone(), true);

                if is_adjustable_slot {
                    return Ok(adjusted_bid);
                }
            }
        }

        Ok(original_bid)
    }
}
