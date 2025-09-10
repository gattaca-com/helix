use std::sync::Arc;

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{mpsc, oneshot};

use crate::{inclusion_lists::MultiRelayInclusionListsService, messages::P2PMessage, P2PApi};

#[derive(Debug)]
pub(crate) enum P2PApiRequest {
    PeerMessage { sender: BlsPublicKeyBytes, message: P2PMessage },
    LocalInclusionList(InclusionListRequest),
    SharedInclusionList(InclusionListRequest),
    FinalInclusionList(InclusionListRequest),
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
        let mut inclusion_lists_service = MultiRelayInclusionListsService::new(self.clone());

        while let Some(request) = api_requests_rx.recv().await {
            match request {
                P2PApiRequest::LocalInclusionList(request) => {
                    inclusion_lists_service.handle_local_inclusion_list(request);
                }
                P2PApiRequest::SharedInclusionList(request) => {
                    inclusion_lists_service.handle_shared_inclusion_list(request);
                }
                P2PApiRequest::FinalInclusionList(request) => {
                    inclusion_lists_service.handle_final_inclusion_list(request);
                }
                P2PApiRequest::PeerMessage { sender, message } => {
                    let P2PMessage::InclusionList(il_msg) = message;
                    inclusion_lists_service
                        .handle_peer_inclusion_list(sender, (il_msg.slot, il_msg.inclusion_list));
                }
            }
        }
    }
}
