use std::sync::Arc;

use helix_common::{
    self, beacon_api::PublishBlobsRequest, blob_sidecars::blob_sidecars_from_unblinded_payload,
};
use helix_types::VersionedSignedProposal;
use tracing::{error, info};

use super::ProposerApi;
use crate::Api;

impl<A: Api> ProposerApi<A> {
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
}
