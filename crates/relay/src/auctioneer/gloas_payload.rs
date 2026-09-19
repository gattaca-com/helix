use alloy_primitives::B256;
use helix_types::{Slot, execution_requests_to_gloas};
use tokio::sync::oneshot;
use tracing::warn;

use crate::{
    api::proposer::HeldGloasPayload,
    auctioneer::{bid_adjustor::BidAdjustor, context::Context},
};

impl<B: BidAdjustor> Context<B> {
    /// Looks up the payload a submission held for `block_hash`, converting it to the real Gloas
    /// consensus shape for `submitSignedBeaconBlock`'s envelope construction.
    pub(super) fn handle_take_held_gloas_payload(
        &self,
        block_hash: B256,
        slot: Slot,
        res_tx: oneshot::Sender<Option<HeldGloasPayload>>,
    ) {
        let held = self.payloads.get_for_redemption(&block_hash).and_then(|entry| {
            let gloas_data = entry.gloas_data();
            let payload = match entry
                .execution_payload()
                .to_lighthouse_gloas_payload(slot, &gloas_data.block_access_list)
            {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(%block_hash, %err, "failed to convert held payload to Gloas shape");
                    return None;
                }
            };
            let execution_requests = execution_requests_to_gloas(
                entry.bid_data_ref().execution_requests,
                &gloas_data.builder_deposits,
                &gloas_data.builder_exits,
            );
            Some(HeldGloasPayload {
                payload,
                execution_requests,
                blobs: entry.payload_and_blobs().blobs_bundle.as_ref().clone(),
            })
        });

        let _ = res_tx.send(held);
    }
}
