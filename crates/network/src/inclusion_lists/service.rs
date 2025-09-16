use std::{collections::HashMap, sync::Arc, time::Duration};

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tracing::{error, trace, warn};

use crate::{
    event_handlers::{InclusionListEvent, InclusionListEventInfo, NetworkEvent},
    inclusion_lists::consensus,
    messages::{InclusionListMessage, InclusionListMessageInfo, NetworkMessage},
    RelayNetworkManager,
};

pub(crate) struct MultiRelayInclusionListsService {
    network_api: Arc<RelayNetworkManager>,
    cutoff_1: Duration,
    cutoff_2: Duration,

    last_slot: u64,

    local_ils: HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
    shared_ils: HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
}

impl MultiRelayInclusionListsService {
    pub(crate) fn new(network_api: Arc<RelayNetworkManager>) -> Self {
        Self {
            cutoff_1: Duration::from_millis(network_api.network_config.cutoff_1_ms),
            cutoff_2: Duration::from_millis(network_api.network_config.cutoff_2_ms),
            local_ils: HashMap::with_capacity(network_api.network_config.peers.len()),
            shared_ils: HashMap::with_capacity(network_api.network_config.peers.len()),
            last_slot: 0,
            network_api,
        }
    }

    pub(crate) fn handle(&mut self, event: InclusionListEvent) {
        match event {
            InclusionListEvent::Local(event) => self.handle_local_inclusion_list(event),
            InclusionListEvent::Shared(event) => self.handle_shared_inclusion_list(event),
            InclusionListEvent::Final(event) => self.handle_final_inclusion_list(event),
        }
    }

    pub(crate) fn handle_local_inclusion_list(&mut self, event: InclusionListEventInfo) {
        // Check event's slot is not in the past
        if self.last_slot > event.slot {
            warn!(slot=%event.slot, last_slot=%self.last_slot, "got local inclusion list event for a past slot, skipping");
            let _ = event.result_tx.send(Some(event.inclusion_list));
            return;
        }

        // Compute duration until first cutoff points
        let duration_into_slot =
            self.network_api.signing_context.context.duration_into_slot(event.slot.into());

        // Check the slot is not in the future
        if duration_into_slot.is_none() {
            warn!("got inclusion list for a slot in the future, skipping");
            let _ = event.result_tx.send(Some(event.inclusion_list));
            return;
        }
        let sleep_time = self.cutoff_1.saturating_sub(duration_into_slot.unwrap());

        // If event was too late into the slot, ignore it
        if sleep_time.is_zero() {
            warn!("got inclusion list too late into the slot, skipping");
            let _ = event.result_tx.send(Some(event.inclusion_list));
            return;
        }
        self.last_slot = event.slot;

        let msg = InclusionListMessageInfo::new(event.slot, event.inclusion_list.clone());

        trace!(slot=%event.slot, "broadcasting local inclusion list");
        self.network_api.broadcast(NetworkMessage::InclusionList(InclusionListMessage::Local(msg)));

        let api_events_tx = self.network_api.api_events_tx.clone();

        // Spawn a task that sleeps until cutoff time and advances us to the next step
        tokio::spawn(async move {
            // Sleep until t_1 time
            tokio::time::sleep(sleep_time).await;
            let shared_il_msg = NetworkEvent::InclusionList(InclusionListEvent::Shared(event));
            let _ = api_events_tx
                .send(shared_il_msg)
                .await
                .inspect_err(|e| error!(err=?e, "failed to send SharedInclusionList"));
        });
    }

    pub(crate) fn handle_shared_inclusion_list(&mut self, event: InclusionListEventInfo) {
        if self.last_slot > event.slot {
            warn!(slot=%event.slot, last_slot=%self.last_slot, "got shared inclusion list event for a past slot, skipping");
            let _ = event.result_tx.send(Some(event.inclusion_list));
            return;
        }
        let InclusionListEventInfo { slot, inclusion_list, .. } = event;
        trace!(local_ils_count=%self.local_ils.len(), "computing shared inclusion list");

        let shared_il =
            consensus::compute_shared_inclusion_list(&self.local_ils, slot, inclusion_list);
        self.local_ils.clear();

        let msg = InclusionListMessageInfo::new(slot, shared_il.clone());
        self.network_api
            .broadcast(NetworkMessage::InclusionList(InclusionListMessage::Shared(msg)));

        let api_events_tx = self.network_api.api_events_tx.clone();

        // Compute duration until cutoff 2
        let duration_into_slot =
            self.network_api.signing_context.context.duration_into_slot(event.slot.into());
        let sleep_time = self.cutoff_2.saturating_sub(duration_into_slot.unwrap_or_default());

        if sleep_time.is_zero() {
            warn!("got shared inclusion list too late into the slot, skipping");
            let _ = event.result_tx.send(Some(shared_il));
            return;
        }
        let settle_event = InclusionListEventInfo { inclusion_list: shared_il, ..event };

        // Spawn a task that sleeps until cutoff time and advances us to the next step
        tokio::spawn(async move {
            // Sleep until t_2 time
            tokio::time::sleep(sleep_time).await;
            let shared_il_msg =
                NetworkEvent::InclusionList(InclusionListEvent::Final(settle_event));
            let _ = api_events_tx
                .send(shared_il_msg)
                .await
                .inspect_err(|e| error!(err=?e, "failed to send FinalInclusionList"));
        });
    }

    pub(crate) fn handle_final_inclusion_list(&mut self, event: InclusionListEventInfo) {
        if self.last_slot > event.slot {
            warn!(slot=%event.slot, last_slot=%self.last_slot, "got final inclusion list event for a past slot, skipping");
            let _ = event.result_tx.send(Some(event.inclusion_list));
            return;
        }
        let InclusionListEventInfo { slot, inclusion_list, result_tx } = event;
        trace!(shared_ils_count=%self.shared_ils.len(), "computing final inclusion list");

        let final_il =
            consensus::compute_final_inclusion_list(&mut self.shared_ils, slot, inclusion_list);

        self.shared_ils.clear();

        // Send result back to eventer
        let _ = result_tx
            .send(Some(final_il))
            .inspect_err(|e| error!(err=?e, "failed to send final inclusion list response"));
    }

    pub(crate) fn handle_peer_message(
        &mut self,
        sender: BlsPublicKeyBytes,
        il_msg: InclusionListMessage,
    ) {
        match il_msg {
            InclusionListMessage::Local(il_msg) => {
                self.handle_peer_local_inclusion_list(sender, il_msg)
            }
            InclusionListMessage::Shared(il_msg) => {
                self.handle_peer_shared_inclusion_list(sender, il_msg)
            }
        }
    }

    pub(crate) fn handle_peer_local_inclusion_list(
        &mut self,
        sender: BlsPublicKeyBytes,
        il_msg: InclusionListMessageInfo,
    ) {
        if self.last_slot > il_msg.slot {
            warn!(slot=%il_msg.slot, last_slot=%self.last_slot, "got local inclusion list from peer for a past slot, skipping");
            return;
        }
        trace!(peer=%sender, "got local IL from peer");
        self.local_ils.insert(sender, (il_msg.slot, il_msg.inclusion_list));
    }

    pub(crate) fn handle_peer_shared_inclusion_list(
        &mut self,
        sender: BlsPublicKeyBytes,
        il_msg: InclusionListMessageInfo,
    ) {
        if self.last_slot > il_msg.slot {
            warn!(slot=%il_msg.slot, last_slot=%self.last_slot, "got shared inclusion list from peer for a past slot, skipping");
            return;
        }
        trace!(peer=%sender, "got shared IL from peer");
        self.shared_ils.insert(sender, (il_msg.slot, il_msg.inclusion_list));
    }
}
