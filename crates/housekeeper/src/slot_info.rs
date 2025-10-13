use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{B256, Bytes};
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponse, BuilderGetValidatorsResponseEntry},
    chain_info::ChainInfo,
};
use helix_types::Slot;
use parking_lot::RwLock;
use tracing::{error, info};

use crate::{PayloadAttributesUpdate, SlotUpdate};

/// All information that is refreshed every slot. This is thread safe and can be cloned/shared
#[derive(Clone)]
pub struct CurrentSlotInfo {
    /// Information about the current head slot and next proposer duty
    // TODO: split this into a AtomicU64?
    curr_slot_info: Arc<RwLock<(u64, Option<BuilderGetValidatorsResponseEntry>)>>,
    proposer_duties_response: Arc<RwLock<Option<Bytes>>>,
    /// Parent hash / slot -> payload attributes
    payload_attributes: Arc<RwLock<HashMap<(B256, Slot), PayloadAttributesUpdate>>>,
}

impl Default for CurrentSlotInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl CurrentSlotInfo {
    pub fn new() -> Self {
        Self {
            curr_slot_info: Arc::new(RwLock::new((0, None))),
            proposer_duties_response: Arc::new(RwLock::new(None)),
            payload_attributes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn head_slot(&self) -> Slot {
        self.curr_slot_info.read().0.into()
    }

    pub fn slot_info(&self) -> (Slot, Option<BuilderGetValidatorsResponseEntry>) {
        let (slot, resp) = self.curr_slot_info.read().clone();
        (slot.into(), resp)
    }

    pub fn proposer_duties_response(&self) -> Option<Bytes> {
        self.proposer_duties_response.read().clone()
    }

    pub fn payload_attributes(
        &self,
        parent_hash: B256,
        slot: Slot,
    ) -> Option<PayloadAttributesUpdate> {
        self.payload_attributes.read().get(&(parent_hash, slot)).cloned()
    }

    /// Handle a new slot update.
    /// Updates the next proposer duty and prepares the get_validators() response.
    pub fn handle_new_slot(&self, slot_update: SlotUpdate, chain_info: &ChainInfo) {
        let epoch = slot_update.slot.epoch(chain_info.slots_per_epoch());

        info!(
            %epoch,
            slot_head = %slot_update.slot,
            slot_start_next_epoch = %(epoch + 1).start_slot(chain_info.slots_per_epoch()),
            next_proposer_duty = ?slot_update.next_duty,
            "updated head slot",
        );

        *self.curr_slot_info.write() = (slot_update.slot.as_u64(), slot_update.next_duty);

        if let Some(new_duties) = slot_update.new_duties {
            let response: Vec<BuilderGetValidatorsResponse> =
                new_duties.into_iter().map(|duty| duty.into()).collect();
            match serde_json::to_vec(&response) {
                Ok(duty_bytes) => {
                    *self.proposer_duties_response.write() = Some(Bytes::from(duty_bytes))
                }
                Err(err) => {
                    error!(%err, "failed to serialize proposer duties to JSON");
                    *self.proposer_duties_response.write() = None;
                }
            }
        }
    }

    pub fn handle_new_payload_attributes(&self, payload_attributes: PayloadAttributesUpdate) {
        let head_slot = self.head_slot().as_u64();

        if payload_attributes.slot <= head_slot {
            return;
        }

        info!(
            slot = %payload_attributes.slot,
            randao = ?payload_attributes.payload_attributes.prev_randao,
            timestamp = payload_attributes.payload_attributes.timestamp,
            "updated payload attributes",
        );

        // Discard payload attributes if already known
        let payload_attributes_key = &(payload_attributes.parent_hash, payload_attributes.slot);
        let mut all_payload_attributes = self.payload_attributes.write();
        if all_payload_attributes.contains_key(payload_attributes_key) {
            return;
        }

        // Clean up old payload attributes
        all_payload_attributes.retain(|_, value| value.slot >= head_slot);

        // Save new one
        all_payload_attributes.insert(*payload_attributes_key, payload_attributes);
    }
}
