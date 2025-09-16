use std::sync::Arc;

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{mpsc, oneshot};

use crate::{
    inclusion_lists::service::MultiRelayInclusionListsService, messages::NetworkMessage,
    RelayNetworkApi,
};

#[derive(Debug)]
pub(crate) enum NetworkEvent {
    PeerMessage { sender: BlsPublicKeyBytes, message: NetworkMessage },
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

impl RelayNetworkApi {
    pub(crate) async fn handle_requests(
        self: Arc<Self>,
        mut api_requests_rx: mpsc::Receiver<NetworkEvent>,
    ) {
        let mut inclusion_lists_service = MultiRelayInclusionListsService::new(self.clone());

        while let Some(request) = api_requests_rx.recv().await {
            match request {
                NetworkEvent::LocalInclusionList(request) => {
                    inclusion_lists_service.handle_local_inclusion_list(request);
                }
                NetworkEvent::SharedInclusionList(request) => {
                    inclusion_lists_service.handle_shared_inclusion_list(request);
                }
                NetworkEvent::FinalInclusionList(request) => {
                    inclusion_lists_service.handle_final_inclusion_list(request);
                }
                NetworkEvent::PeerMessage { message, sender } => match message {
                    NetworkMessage::LocalInclusionList(il_msg) => {
                        inclusion_lists_service.handle_peer_local_inclusion_list(sender, il_msg);
                    }
                    NetworkMessage::SharedInclusionList(il_msg) => {
                        inclusion_lists_service.handle_peer_shared_inclusion_list(sender, il_msg);
                    }
                },
            }
        }
    }
}
