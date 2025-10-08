use helix_common::api::proposer_api::GetHeaderParams;
use tokio::sync::oneshot;
use tracing::error;

use crate::{
    auctioneer::{context::Context, types::GetHeaderResult},
    proposer::ProposerApiError,
    Api,
};

impl<A: Api> Context<A> {
    pub(super) fn handle_get_header(
        &self,
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        let res = self.get_header(params);
        let _ = res_tx.send(res);
    }

    fn get_header(&self, params: GetHeaderParams) -> GetHeaderResult {
        if params.slot != self.bid_slot.as_u64() {
            return Err(ProposerApiError::RequestWrongSlot {
                request_slot: params.slot,
                bid_slot: self.bid_slot.as_u64(),
            });
        }

        let Some(best_block_hash) = self.bid_sorter.get_header() else {
            return Err(ProposerApiError::NoBidPrepared);
        };

        let Some(entry) = self.payloads.get(&best_block_hash) else {
            error!("failed to get payload from bid sorter best, this should never happen!");
            return Err(ProposerApiError::NoBidPrepared);
        };

        let Some(data) = entry.to_header_data() else {
            error!("failed to get header data from payload entry, this should never happen!");
            return Err(ProposerApiError::NoBidPrepared);
        };

        Ok(data)
    }
}
