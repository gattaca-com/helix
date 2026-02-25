use helix_common::{GossipedPayloadTrace, utils::utcnow_ns};
use tracing::{debug, error};
use uuid::Uuid;

use super::api::BuilderApi;
use crate::{api::Api, gossip::BroadcastPayloadParams};

// Handle Gossiped Payloads
impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        let block_hash = req.execution_payload.execution_payload.block_hash;

        debug!(?block_hash, "received gossiped payload");

        let trace = GossipedPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        if self.auctioneer_handle.gossip_payload(req).is_err() {
            error!("failed sending gossip payload to auctioneer");
        }

        // Save gossiped payload trace to db
        self.db.save_gossiped_payload_trace(block_hash, trace);
    }
}
