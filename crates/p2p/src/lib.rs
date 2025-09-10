use std::{collections::HashMap, sync::Arc, time::Duration};

use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PPeerConfig};
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{error, warn};

use crate::{
    il_consensus::{compute_final_inclusion_list, compute_shared_inclusion_list},
    messages::{InclusionListMessage, P2PMessage},
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

#[derive(Debug)]
enum P2PApiRequest {
    PeerMessage { sender: BlsPublicKeyBytes, message: P2PMessage },
    LocalInclusionList(InclusionListRequest),
    SharedInclusionList(InclusionListRequest),
    SettledInclusionList(InclusionListRequest),
}

#[derive(Debug)]
struct InclusionListRequest {
    pub(crate) slot: u64,
    pub(crate) inclusion_list: InclusionList,
    pub(crate) result_tx: oneshot::Sender<Option<InclusionList>>,
}

impl P2PApi {
    async fn handle_requests(self: Arc<Self>, mut api_requests_rx: mpsc::Receiver<P2PApiRequest>) {
        const CUTOFF_TIME_1: Duration = Duration::from_secs(2);
        const CUTOFF_TIME_2: Duration = Duration::from_secs(2);
        let mut vote_map: HashMap<BlsPublicKeyBytes, (u64, InclusionList)> = HashMap::new();
        while let Some(request) = api_requests_rx.recv().await {
            match request {
                P2PApiRequest::LocalInclusionList(request) => {
                    let msg =
                        InclusionListMessage::new(request.slot, request.inclusion_list.clone());
                    self.broadcast(msg.into());

                    let api_requests_tx = self.api_requests_tx.clone();
                    let shared_il_msg = P2PApiRequest::SharedInclusionList(request);
                    tokio::spawn(async move {
                        // Sleep for t_1 time
                        tokio::time::sleep(CUTOFF_TIME_1).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SharedInclusionList(request) => {
                    let InclusionListRequest { slot, inclusion_list, .. } = request;
                    let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list);

                    let msg = InclusionListMessage::new(slot, shared_il.clone());
                    self.broadcast(msg.into());

                    let api_requests_tx = self.api_requests_tx.clone();
                    let settle_request =
                        InclusionListRequest { inclusion_list: shared_il, ..request };
                    let shared_il_msg = P2PApiRequest::SettledInclusionList(settle_request);
                    tokio::spawn(async move {
                        // Sleep for t_2 time
                        tokio::time::sleep(CUTOFF_TIME_2).await;
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SettledInclusionList(request) => {
                    let InclusionListRequest { slot, inclusion_list, result_tx } = request;
                    let vote_map = std::mem::take(&mut vote_map);
                    let final_il = compute_final_inclusion_list(vote_map, slot, inclusion_list);
                    let _ = result_tx
                        .send(Some(final_il))
                        .inspect_err(|e| error!(err=?e, "failed to send settled inclusion list"));
                }
                P2PApiRequest::PeerMessage { sender, message } => {
                    let P2PMessage::InclusionList(il_msg) = message;
                    vote_map.insert(sender, (il_msg.slot, il_msg.inclusion_list));
                }
            }
        }
    }
}
