use helix_common::api::proposer_api::GetHeaderParams;
use tokio::sync::oneshot;
use tracing::error;

use crate::api::{
    auctioneer::{context::Context, types::GetHeaderResult},
    proposer::ProposerApiError,
};

impl Context {
    pub(super) fn handle_get_header(
        &self,
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        assert_eq!(params.slot, self.bid_slot.as_u64(), "params should already be validated!");
        let _ = res_tx.send(self.get_header());
    }

    fn get_header(&self) -> GetHeaderResult {
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
