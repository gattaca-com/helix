use alloy_primitives::B256;
use tokio::sync::oneshot;

use crate::{
    api::proposer::HeldGloasPayload,
    auctioneer::{bid_adjustor::BidAdjustor, context::Context},
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_take_held_gloas_payload(
        &self,
        block_hash: B256,
        res_tx: oneshot::Sender<Option<HeldGloasPayload>>,
    ) {
        let held = self
            .gloas_payloads
            .get_for_redemption(&block_hash)
            .map(|entry| HeldGloasPayload { submission: entry.submission.clone() });

        let _ = res_tx.send(held);
    }
}
