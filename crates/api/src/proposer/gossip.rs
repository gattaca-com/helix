use std::sync::Arc;

use helix_common::{
    self, beacon_api::PublishBlobsRequest, blob_sidecars::blob_sidecars_from_unblinded_payload,
    metadata_provider::MetadataProvider, metrics::PROPOSER_GOSSIP_QUEUE, task, utils::utcnow_ns,
    GetPayloadTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::VersionedSignedProposal;
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error, info};
use uuid::Uuid;

use super::ProposerApi;
use crate::gossiper::{
    traits::GossipClientTrait,
    types::{BroadcastPayloadParams, GossipedMessage},
};

impl<A, DB, G, MP> ProposerApi<A, DB, G, MP>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    G: GossipClientTrait + 'static,
    MP: MetadataProvider + 'static,
{
    /// If there are blobs in the unblinded payload, this function will send them directly to the
    /// beacon chain to be propagated async to the full block.
    pub(super) async fn gossip_blobs(&self, unblinded_payload: Arc<VersionedSignedProposal>) {
        let blob_sidecars = match blob_sidecars_from_unblinded_payload(&unblinded_payload) {
            Ok(blob_sidecars) => blob_sidecars,
            Err(err) => {
                error!(?err, "gossip blobs: failed to build blob sidecars for async gossiping");
                return;
            }
        };

        info!("gossip blobs: successfully built blob sidecars for request. Gossiping async..");

        // Send blob sidecars to beacon clients.
        let publish_blob_request = PublishBlobsRequest {
            blob_sidecars,
            beacon_root: unblinded_payload.signed_block.parent_root(),
        };
        if let Err(error) = self.multi_beacon_client.publish_blobs(publish_blob_request).await {
            error!(?error, "gossip blobs: failed to gossip blob sidecars");
        }
    }

    /// This function should be run as a seperate async task.
    /// Will process new gossiped messages from
    pub(super) async fn process_gossiped_info(&self, mut recveiver: Receiver<GossipedMessage>) {
        while let Some(msg) = recveiver.recv().await {
            PROPOSER_GOSSIP_QUEUE.dec();
            match msg {
                GossipedMessage::GetPayload(payload) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        let mut trace =
                            GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };
                        debug!(request_id = %payload.request_id, "processing gossiped payload");
                        match api_clone
                            ._get_payload(payload.signed_blinded_beacon_block, &mut trace, None)
                            .await
                        {
                            Ok(_get_payload_response) => {
                                debug!(request_id = %payload.request_id, "gossiped payload processed");
                            }
                            Err(err) => {
                                error!(request_id = %payload.request_id, error = %err, "error processing gossiped payload");
                            }
                        }
                    });
                }
                GossipedMessage::RequestPayload(payload) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        let request_id = Uuid::new_v4();
                        debug!(request_id = %request_id, "processing gossiped payload request");
                        let payload_result = api_clone
                            .get_execution_payload(
                                payload.slot,
                                &payload.proposer_pub_key,
                                &payload.block_hash,
                                false,
                            )
                            .await;

                        match payload_result {
                            Ok(execution_payload) => {
                                if let Err(err) = api_clone
                                    .gossiper
                                    .broadcast_payload(BroadcastPayloadParams {
                                        execution_payload,
                                        slot: payload.slot,
                                        proposer_pub_key: payload.proposer_pub_key,
                                    })
                                    .await
                                {
                                    error!(request_id = %request_id, error = %err, "error broadcasting payload");
                                }
                            }
                            Err(err) => {
                                error!(request_id = %request_id, error = %err, "error fetching execution payload");
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    }
}
