use std::sync::Arc;

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{mpsc, oneshot};

use crate::{
    inclusion_lists::service::MultiRelayInclusionListsService, messages::NetworkMessage,
    RelayNetworkManager,
};

#[derive(Debug)]
pub(crate) enum NetworkEvent {
    PeerMessage { sender: BlsPublicKeyBytes, message: NetworkMessage },
    LocalInclusionList(InclusionListEvent),
    SharedInclusionList(InclusionListEvent),
    FinalInclusionList(InclusionListEvent),
}

#[derive(Debug)]
pub(crate) struct InclusionListEvent {
    pub(crate) slot: u64,
    pub(crate) inclusion_list: InclusionList,
    pub(crate) result_tx: oneshot::Sender<Option<InclusionList>>,
}

impl RelayNetworkManager {
    pub(crate) async fn event_handling_loop(
        self: Arc<Self>,
        mut events_rx: mpsc::Receiver<NetworkEvent>,
    ) {
        let mut inclusion_lists_service = MultiRelayInclusionListsService::new(self.clone());

        while let Some(event) = events_rx.recv().await {
            match event {
                NetworkEvent::LocalInclusionList(event) => {
                    inclusion_lists_service.handle_local_inclusion_list(event);
                }
                NetworkEvent::SharedInclusionList(event) => {
                    inclusion_lists_service.handle_shared_inclusion_list(event);
                }
                NetworkEvent::FinalInclusionList(event) => {
                    inclusion_lists_service.handle_final_inclusion_list(event);
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
