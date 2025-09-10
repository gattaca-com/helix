use std::sync::Arc;

use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PPeerConfig};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use crate::{
    handlers::{InclusionListRequest, P2PApiRequest},
    messages::P2PMessage,
};

mod handlers;
pub(crate) mod il_consensus;
pub mod messages;
mod socket;

pub struct P2PApi {
    broadcast_tx: broadcast::Sender<P2PMessage>,
    api_requests_tx: mpsc::Sender<P2PApiRequest>,
    signing_context: Arc<RelaySigningContext>,
    peer_configs: Vec<P2PPeerConfig>,
}

impl P2PApi {
    fn broadcast(&self, message: P2PMessage) {
        // Ignore error if there are no active receivers.
        let _ = self.broadcast_tx.send(message);
    }

    pub async fn share_inclusion_list(
        &self,
        slot: u64,
        inclusion_list: InclusionList,
    ) -> Option<InclusionList> {
        let (result_tx, result_rx) = oneshot::channel();
        let request = P2PApiRequest::LocalInclusionList(InclusionListRequest {
            slot,
            inclusion_list,
            result_tx,
        });
        // Send request to P2P API
        if let Err(err) = self.api_requests_tx.send(request).await {
            // If API service is unavailable, just return the original IL and log a warning
            warn!("failed to send inclusion list to P2P API");
            match err.0 {
                P2PApiRequest::LocalInclusionList(request) => {
                    return Some(request.inclusion_list);
                }
                _ => unreachable!("the returned value is an inclusion list request"),
            }
        }
        // If API service drops the channel, return None and log a warning
        result_rx.await.inspect_err(|_| warn!("response channel was dropped")).ok().flatten()
    }
}
