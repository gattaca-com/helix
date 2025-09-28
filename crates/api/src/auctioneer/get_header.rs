use alloy_primitives::U256;
use tokio::sync::oneshot;

use crate::{
    auctioneer::{types::GetHeaderResult, SortingData},
    proposer::{GetHeaderParams, ProposerApiError},
};

impl SortingData {
    pub(super) fn handle_get_header(
        &self,
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    ) {
        let res = self.get_header(params);
        let _ = res_tx.send(res);
    }

    fn get_header(&self, params: GetHeaderParams) -> GetHeaderResult {
        if params.slot != self.slot.bid_slot.as_u64() {
            return Err(ProposerApiError::RequestWrongSlot {
                request_slot: params.slot,
                bid_slot: self.slot.bid_slot.as_u64(),
            });
        }

        let Some(bid) = self.sort.get_header() else {
            return Err(ProposerApiError::NoBidPrepared);
        };

        // TODO: this check is proably useless
        if bid.value == U256::ZERO {
            return Err(ProposerApiError::BidValueZero);
        }

        Ok(bid)
    }
}
