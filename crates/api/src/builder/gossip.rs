use helix_common::{task, utils::utcnow_ns, GossipedPayloadTrace};
use helix_database::DatabaseService;
use tracing::{debug, error};
use uuid::Uuid;

use super::api::BuilderApi;
use crate::{gossiper::types::BroadcastPayloadParams, Api};

// Handle Gossiped Payloads
impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        todo!();

        let block_hash = req.execution_payload.execution_payload.block_hash;

        debug!(?block_hash, "received gossiped payload");

        let mut trace = GossipedPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        // Verify that the gossiped payload is not for a past slot
        let head_slot = self.curr_slot_info.head_slot();
        if req.slot <= head_slot.as_u64() {
            debug!("received gossiped payload for a past slot");
            return;
        }

        trace.pre_checks = utcnow_ns();

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
}
