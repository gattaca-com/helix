use std::{collections::HashMap, sync::Arc, time::Duration};

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{mpsc, oneshot};
use tracing::error;

use crate::{
    il_consensus::{compute_final_inclusion_list, compute_shared_inclusion_list},
    messages::{InclusionListMessage, P2PMessage},
    P2PApi,
};

#[derive(Debug)]
pub(crate) enum P2PApiRequest {
    PeerMessage { sender: BlsPublicKeyBytes, message: P2PMessage },
    LocalInclusionList(InclusionListRequest),
    SharedInclusionList(InclusionListRequest),
    SettledInclusionList(InclusionListRequest),
}

#[derive(Debug)]
pub(crate) struct InclusionListRequest {
    pub(crate) slot: u64,
    pub(crate) inclusion_list: InclusionList,
    pub(crate) result_tx: oneshot::Sender<Option<InclusionList>>,
}

impl P2PApi {
    pub(crate) async fn handle_requests(
        self: Arc<Self>,
        mut api_requests_rx: mpsc::Receiver<P2PApiRequest>,
    ) {
        let cutoff_time_1 = Duration::from_millis(self.p2p_config.cutoff_1_ms);
        let cutoff_time_2 = Duration::from_millis(self.p2p_config.cutoff_2_ms);
        let mut vote_map: HashMap<BlsPublicKeyBytes, (u64, InclusionList)> = HashMap::new();

        while let Some(request) = api_requests_rx.recv().await {
            match request {
                P2PApiRequest::LocalInclusionList(request) => {
                    let msg =
                        InclusionListMessage::new(request.slot, request.inclusion_list.clone());
                    self.broadcast(msg.into());

                    let api_requests_tx = self.api_requests_tx.clone();

                    // Compute duration until cutoff 1
                    let duration_into_slot =
                        self.signing_context.context.duration_into_slot(request.slot.into());
                    let sleep_time =
                        cutoff_time_1.saturating_sub(duration_into_slot.unwrap_or_default());

                    // Spawn a task that sleeps until cutoff time and advances us to the next step
                    tokio::spawn(async move {
                        // Sleep until t_1 time
                        tokio::time::sleep(sleep_time).await;
                        let shared_il_msg = P2PApiRequest::SharedInclusionList(request);
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

                    // Compute duration until cutoff 2
                    let duration_into_slot =
                        self.signing_context.context.duration_into_slot(request.slot.into());
                    let sleep_time =
                        cutoff_time_2.saturating_sub(duration_into_slot.unwrap_or_default());

                    let settle_request =
                        InclusionListRequest { inclusion_list: shared_il, ..request };

                    // Spawn a task that sleeps until cutoff time and advances us to the next step
                    tokio::spawn(async move {
                        // Sleep until t_2 time
                        tokio::time::sleep(sleep_time).await;
                        let shared_il_msg = P2PApiRequest::SettledInclusionList(settle_request);
                        let _ = api_requests_tx.send(shared_il_msg).await.inspect_err(
                            |e| error!(err=?e, "failed to send shared inclusion list"),
                        );
                    });
                }
                P2PApiRequest::SettledInclusionList(request) => {
                    let InclusionListRequest { slot, inclusion_list, result_tx } = request;
                    let vote_map = std::mem::take(&mut vote_map);
                    let final_il = compute_final_inclusion_list(vote_map, slot, inclusion_list);

                    // Send result back to requester
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
