use std::time::{Duration, Instant};

use alloy_primitives::B256;
use helix_common::GetPayloadTrace;
use helix_types::{
    GetPayloadResponse, SignedBidSubmission, SignedBlindedBeaconBlock, VersionedSignedProposal,
};
use tokio::sync::oneshot;

use crate::{
    builder::simulator_2::{
        worker::{GetPayloadResult, GetPayloadResultData},
        Context, PendingPayload, SortingData,
    },
    proposer::ProposerApiError,
    Api,
};

impl SortingData {
    pub(super) fn handle_get_payload<A: Api>(
        &mut self,
        block_hash: B256,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
        ctx: &mut Context<A>,
    ) {
        if let Some(payload) = self.payloads.get(&block_hash) {
            let res = self._get_payload(blinded, payload, trace).map(
                |(to_proposer, to_publish, trace)| {
                    let proposer_pubkey =
                        self.slot.registration_data.entry.registration.message.pubkey;

                    GetPayloadResultData {
                        to_proposer,
                        to_publish,
                        trace,
                        proposer_pubkey,
                        fork: self.slot.current_fork,
                    }
                },
            );

            let _ = res_tx.send(res);
        } else {
            // we may still receive the payload from builder / gossip, save request for
            // later
            ctx.pending_payloads = Some(PendingPayload {
                block_hash,
                blinded,
                res_tx,
                retry_at: Instant::now() + Duration::from_millis(20),
            });
        }
    }

    fn _get_payload(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: &SignedBidSubmission,
        trace: GetPayloadTrace,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal, GetPayloadTrace), ProposerApiError>
    {
        // TODO: use trace
        let (to_proposer, to_publish) = self.validate_and_unblind(blinded, local)?;
        Ok((to_proposer, to_publish, trace))
    }
}
