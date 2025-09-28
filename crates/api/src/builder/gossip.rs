use helix_common::{task, utils::utcnow_ns, GossipedPayloadTrace};
use helix_database::DatabaseService;
use tracing::{debug, error};
use uuid::Uuid;

use super::api::BuilderApi;
use crate::{builder::simulator_2::Event, gossiper::types::BroadcastPayloadParams, Api};

// Handle Gossiped Payloads
impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        let block_hash = req.execution_payload.execution_payload.block_hash;

        debug!(?block_hash, "received gossiped payload");

        let trace = GossipedPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        if let Err(err) = self.auctioneer_tx.send(Event::GossipPayload(req)) {
            error!(%err, "failed sending gossip payload to auctioneer");
        }

        // Save gossiped payload trace to db
        let db = self.db.clone();
        task::spawn(file!(), line!(), async move {
            if let Err(err) = db.save_gossiped_payload_trace(block_hash, trace).await {
                error!(%err, "failed to store gossiped payload trace")
            }
        });
    }
}
