use helix_common::{bid_submission::BidSubmission, task, utils::utcnow_ns, GossipedPayloadTrace};
use helix_database::DatabaseService;
use helix_types::{ForkName, PayloadAndBlobsRef, SignedBidSubmission};
use tracing::{debug, error};
use uuid::Uuid;

use super::api::BuilderApi;
use crate::{gossiper::types::BroadcastPayloadParams, Api};

// Handle Gossiped Payloads
impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        let block_hash = req.execution_payload.execution_payload.block_hash;

        debug!(?block_hash, "received gossiped payload");

        let mut trace = GossipedPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        // Verify that the gossiped payload is not for a past slot
        let head_slot = self.curr_slot_info.head_slot();
        if req.slot <= head_slot.as_u64() {
            debug!("received gossiped payload for a past slot");
            return;
        }

        // Verify payload has not already been delivered
        if let Some(slot) = self.auctioneer.get_last_slot_delivered() {
            if req.slot <= slot {
                debug!("payload already delivered");
                return;
            }
        }

        trace.pre_checks = utcnow_ns();

        // Save payload to auctioneer
        self.auctioneer.save_execution_payload(
            req.slot,
            &req.proposer_pub_key,
            &(*req.execution_payload.execution_payload.block_hash).into(),
            req.execution_payload,
        );

        trace.auctioneer_update = utcnow_ns();

        debug!("succesfully saved gossiped payload");

        // Save gossiped payload trace to db
        let db = self.db.clone();
        task::spawn(file!(), line!(), async move {
            if let Err(err) = db.save_gossiped_payload_trace(block_hash, trace).await {
                error!(%err, "failed to store gossiped payload trace")
            }
        });
    }

    pub(crate) async fn gossip_payload(
        &self,
        payload: &SignedBidSubmission,
        execution_payload: PayloadAndBlobsRef<'_>,
        fork_name: ForkName,
    ) {
        let params = BroadcastPayloadParams::to_proto(
            execution_payload,
            payload.slot().as_u64(),
            payload.proposer_public_key(),
            fork_name,
        );
        self.gossiper.broadcast_payload(params).await
    }
}
