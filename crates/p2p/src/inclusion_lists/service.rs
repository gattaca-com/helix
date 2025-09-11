use std::{collections::HashMap, sync::Arc, time::Duration};

use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tracing::{error, trace, warn};

use crate::{
    inclusion_lists::consensus,
    messages::{InclusionListMessage, P2PMessage},
    request_handlers::{InclusionListRequest, P2PApiRequest},
    P2PApi,
};

pub(crate) struct MultiRelayInclusionListsService {
    p2p_api: Arc<P2PApi>,
    cutoff_1: Duration,
    cutoff_2: Duration,

    last_slot: u64,

    local_ils: HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
    shared_ils: HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
}

impl MultiRelayInclusionListsService {
    pub(crate) fn new(p2p_api: Arc<P2PApi>) -> Self {
        Self {
            cutoff_1: Duration::from_millis(p2p_api.p2p_config.cutoff_1_ms),
            cutoff_2: Duration::from_millis(p2p_api.p2p_config.cutoff_2_ms),
            local_ils: HashMap::with_capacity(p2p_api.p2p_config.peers.len()),
            shared_ils: HashMap::with_capacity(p2p_api.p2p_config.peers.len()),
            last_slot: 0,
            p2p_api,
        }
    }

    pub(crate) fn handle_local_inclusion_list(&mut self, request: InclusionListRequest) {
        // Check request's slot is not in the past
        if self.last_slot > request.slot {
            warn!(slot=%request.slot, last_slot=%self.last_slot, "got local inclusion list request for a past slot, skipping");
            let _ = request.result_tx.send(Some(request.inclusion_list));
            return;
        }

        // Compute duration until first cutoff points
        let duration_into_slot =
            self.p2p_api.signing_context.context.duration_into_slot(request.slot.into());

        // Check the slot is not in the future
        if duration_into_slot.is_none() {
            warn!("got inclusion list for a slot in the future, skipping");
            let _ = request.result_tx.send(Some(request.inclusion_list));
            return;
        }
        let sleep_time = self.cutoff_1.saturating_sub(duration_into_slot.unwrap());

        // If request was too late into the slot, ignore it
        if sleep_time.is_zero() {
            warn!("got inclusion list too late into the slot, skipping");
            let _ = request.result_tx.send(Some(request.inclusion_list));
            return;
        }
        self.last_slot = request.slot;

        let msg = InclusionListMessage::new(request.slot, request.inclusion_list.clone());

        trace!(slot=%request.slot, "broadcasting local inclusion list");
        self.p2p_api.broadcast(P2PMessage::LocalInclusionList(msg));

        let api_requests_tx = self.p2p_api.api_requests_tx.clone();

        // Spawn a task that sleeps until cutoff time and advances us to the next step
        tokio::spawn(async move {
            // Sleep until t_1 time
            tokio::time::sleep(sleep_time).await;
            let shared_il_msg = P2PApiRequest::SharedInclusionList(request);
            let _ = api_requests_tx
                .send(shared_il_msg)
                .await
                .inspect_err(|e| error!(err=?e, "failed to send SharedInclusionList"));
        });
    }

    pub(crate) fn handle_shared_inclusion_list(&mut self, request: InclusionListRequest) {
        if self.last_slot > request.slot {
            warn!(slot=%request.slot, last_slot=%self.last_slot, "got shared inclusion list request for a past slot, skipping");
            let _ = request.result_tx.send(Some(request.inclusion_list));
            return;
        }
        let InclusionListRequest { slot, inclusion_list, .. } = request;
        trace!(local_ils_count=%self.local_ils.len(), "computing shared inclusion list");

        let shared_il =
            consensus::compute_shared_inclusion_list(&self.local_ils, slot, inclusion_list);
        self.local_ils.clear();

        let msg = InclusionListMessage::new(slot, shared_il.clone());
        self.p2p_api.broadcast(P2PMessage::SharedInclusionList(msg));

        let api_requests_tx = self.p2p_api.api_requests_tx.clone();

        // Compute duration until cutoff 2
        let duration_into_slot =
            self.p2p_api.signing_context.context.duration_into_slot(request.slot.into());
        let sleep_time = self.cutoff_2.saturating_sub(duration_into_slot.unwrap_or_default());

        if sleep_time.is_zero() {
            warn!("got shared inclusion list too late into the slot, skipping");
            let _ = request.result_tx.send(Some(shared_il));
            return;
        }
        let settle_request = InclusionListRequest { inclusion_list: shared_il, ..request };

        // Spawn a task that sleeps until cutoff time and advances us to the next step
        tokio::spawn(async move {
            // Sleep until t_2 time
            tokio::time::sleep(sleep_time).await;
            let shared_il_msg = P2PApiRequest::FinalInclusionList(settle_request);
            let _ = api_requests_tx
                .send(shared_il_msg)
                .await
                .inspect_err(|e| error!(err=?e, "failed to send FinalInclusionList"));
        });
    }

    pub(crate) fn handle_final_inclusion_list(&mut self, request: InclusionListRequest) {
        if self.last_slot > request.slot {
            warn!(slot=%request.slot, last_slot=%self.last_slot, "got final inclusion list request for a past slot, skipping");
            let _ = request.result_tx.send(Some(request.inclusion_list));
            return;
        }
        let InclusionListRequest { slot, inclusion_list, result_tx } = request;
        trace!(shared_ils_count=%self.shared_ils.len(), "computing final inclusion list");

        let final_il =
            consensus::compute_final_inclusion_list(&mut self.shared_ils, slot, inclusion_list);

        self.shared_ils.clear();

        // Send result back to requester
        let _ = result_tx
            .send(Some(final_il))
            .inspect_err(|e| error!(err=?e, "failed to send final inclusion list response"));
    }

    pub(crate) fn handle_peer_local_inclusion_list(
        &mut self,
        sender: BlsPublicKeyBytes,
        il_msg: InclusionListMessage,
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
        il_msg: InclusionListMessage,
    ) {
        if self.last_slot > il_msg.slot {
            warn!(slot=%il_msg.slot, last_slot=%self.last_slot, "got shared inclusion list from peer for a past slot, skipping");
            return;
        }
        trace!(peer=%sender, "got shared IL from peer");
        self.shared_ils.insert(sender, (il_msg.slot, il_msg.inclusion_list));
    }
}
