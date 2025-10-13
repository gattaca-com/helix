use std::sync::Arc;

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tokio::sync::{mpsc, oneshot};

use crate::{
    RelayNetworkManager, inclusion_lists::service::MultiRelayInclusionListsService,
    messages::NetworkMessage,
};

#[derive(Debug)]
pub(crate) enum NetworkEvent {
    PeerMessage { sender: BlsPublicKeyBytes, message: NetworkMessage },
    InclusionList(InclusionListEvent),
}

#[derive(Debug)]
pub(crate) enum InclusionListEvent {
    Local(InclusionListEventInfo),
    Shared(InclusionListEventInfo),
    Final(InclusionListEventInfo),
}

#[derive(Debug)]
pub(crate) struct InclusionListEventInfo {
    pub(crate) slot: u64,
    pub(crate) inclusion_list: InclusionList,
    pub(crate) result_tx: oneshot::Sender<Option<InclusionList>>,
}

impl RelayNetworkManager {
    pub(crate) async fn run_event_handling_loop(
        self: Arc<Self>,
        mut events_rx: mpsc::Receiver<NetworkEvent>,
    ) {
        let mut inclusion_lists_handler = MultiRelayInclusionListsService::new(self.clone());

        while let Some(event) = events_rx.recv().await {
            match event {
                NetworkEvent::InclusionList(event) => {
                    inclusion_lists_handler.handle(event);
                }
                NetworkEvent::PeerMessage { message, sender } => match message {
                    NetworkMessage::InclusionList(il_msg) => {
                        inclusion_lists_handler.handle_peer_message(sender, il_msg);
                    }
                },
            }
        }
    }
}
