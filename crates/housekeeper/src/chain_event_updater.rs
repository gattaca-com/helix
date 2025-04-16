use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_primitives::B256;
use helix_beacon::types::{HeadEventData, PayloadAttributes, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry,
    chain_info::ChainInfo,
    utils::{get_payload_attributes_key, utcnow_sec},
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{SlotClockTrait, Withdrawals};
use tokio::{
    sync::broadcast,
    time::{interval_at, sleep, Instant},
};
use tracing::{error, info, warn};
use tree_hash::TreeHash;

// Do not accept slots more than 60 seconds in the future
const MAX_DISTANCE_FOR_FUTURE_SLOT: u64 = 60;
const CUTT_OFF_TIME: u64 = 4;

/// Payload for a new payload attribute event sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct PayloadAttributesUpdate {
    pub slot: u64,
    pub parent_hash: B256,
    pub withdrawals_root: B256,
    pub payload_attributes: PayloadAttributes,
}

/// Payload for head event updates sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct SlotUpdate {
    pub slot: u64,
    pub next_duty: Option<BuilderGetValidatorsResponseEntry>,
    pub new_duties: Option<Vec<BuilderGetValidatorsResponseEntry>>,
}

#[derive(Clone, Debug)]
pub enum ChainUpdate {
    SlotUpdate(Box<SlotUpdate>),
    PayloadAttributesUpdate(PayloadAttributesUpdate),
}

/// Manages the update of head slots and the fetching of new proposer duties.
pub struct ChainEventUpdater<D: DatabaseService, A: Auctioneer> {
    head_slot: u64,
    known_payload_attributes: HashMap<String, PayloadAttributesEvent>,

    proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,

    database: Arc<D>,
    auctioneer: Arc<A>,
    chain_update_tx: broadcast::Sender<ChainUpdate>,
    chain_info: Arc<ChainInfo>,
}

impl<D: DatabaseService, A: Auctioneer> ChainEventUpdater<D, A> {
    pub fn new(
        database: Arc<D>,
        auctioneer: Arc<A>,
        chain_update_tx: broadcast::Sender<ChainUpdate>,
        chain_info: Arc<ChainInfo>,
    ) -> Self {
        Self {
            head_slot: 0,
            known_payload_attributes: Default::default(),
            database,
            auctioneer,
            chain_update_tx,
            proposer_duties: Vec::new(),
            chain_info,
        }
    }

    /// Starts the updater and listens to head events and new subscriptions.
    pub async fn start(
        mut self,
        mut head_event_rx: broadcast::Receiver<HeadEventData>,
        mut payload_attributes_rx: broadcast::Receiver<PayloadAttributesEvent>,
    ) {
        let start_instant = Instant::now() +
            self.chain_info.clock.duration_to_next_slot().unwrap() +
            Duration::from_secs(CUTT_OFF_TIME);
        let mut timer =
            interval_at(start_instant, Duration::from_secs(self.chain_info.seconds_per_slot()));
        loop {
            tokio::select! {
                head_event_result = head_event_rx.recv() => {
                    match head_event_result {
                        Ok(head_event) => {
                            info!(head_slot =% head_event.slot, "Received head event");
                            self.process_slot(head_event.slot.into()).await
                        },
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("head events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("head event channel closed");
                            break;
                        }
                    }
                }
                payload_attributes_result = payload_attributes_rx.recv() => {
                    match payload_attributes_result {
                        Ok(payload_attributes) => self.process_payload_attributes(payload_attributes).await,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("payload attributes events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("payload attributes event channel closed");
                            break;
                        }
                    }
                }

                _ = timer.tick() => {
                    info!("4 seconds into slot. Attempting to slot update...");
                    match self.chain_info.clock.now() {
                        Some(slot) => self.process_slot(slot.as_u64()).await,
                        None => {
                            error!("could not get current slot");
                        }
                    }
                }
            }
        }
    }

    /// Handles a new slot.
    async fn process_slot(&mut self, slot: u64) {
        if self.head_slot >= slot {
            return;
        }

        info!(head_slot =% slot, "Processing slot",);

        // Validate this isn't a faulty head slot

        let slot_timestamp =
            self.chain_info.genesis_time_in_secs + (slot * self.chain_info.seconds_per_slot());
        if slot_timestamp > utcnow_sec() + MAX_DISTANCE_FOR_FUTURE_SLOT {
            warn!(head_slot = slot, "slot is too far in the future",);
            return;
        }

        // Log any missed slots
        if self.head_slot != 0 {
            for s in (self.head_slot + 1)..slot {
                warn!(missed_slot = s, "missed slot");
            }
        }

        self.head_slot = slot;

        // Give housekeeper some time to update proposer duties
        sleep(std::time::Duration::from_secs(1)).await;
        let mut new_duties = match self.database.get_proposer_duties().await {
            Ok(new_duties) => {
                info!(
                    head_slot = slot,
                    new_duties = new_duties.len(),
                    "Fetched proposer duties from db",
                );
                Some(new_duties)
            }
            Err(err) => {
                error!(error = %err, "Failed to get proposer duties from db");
                None
            }
        };

        // Update local cache if new duties were fetched.
        if let Some(new_duties) = &mut new_duties {
            for duty in new_duties.iter_mut() {
                match self
                    .auctioneer
                    .is_primev_proposer(&duty.entry.registration.message.pubkey)
                    .await
                {
                    Ok(is_primev) => {
                        if is_primev {
                            info!(head_slot = slot, "Primev proposer duty found");
                            match &mut duty.entry.preferences.trusted_builders {
                                Some(trusted_builders) => {
                                    trusted_builders.push("PrimevBuilder".to_string());
                                }
                                None => {
                                    duty.entry.preferences.trusted_builders =
                                        Some(vec!["".to_string()]);
                                }
                            }
                        }
                    }
                    Err(err) => {
                        error!(error = %err, "Failed to check if proposer is primev");
                    }
                }
            }

            self.proposer_duties.clone_from(new_duties);
        }

        // Get the next proposer duty for the new slot.
        let next_duty = self.proposer_duties.iter().find(|duty| duty.slot == slot + 1).cloned();

        let update = ChainUpdate::SlotUpdate(Box::new(SlotUpdate { slot, new_duties, next_duty }));
        let _ = self.chain_update_tx.send(update);
    }

    // Handles a new payload attributes event
    async fn process_payload_attributes(&mut self, event: PayloadAttributesEvent) {
        // require new proposal slot in the future
        if self.head_slot >= event.data.proposal_slot.as_u64() {
            return;
        }

        // Discard payload attributes if already known
        let payload_attributes_key =
            get_payload_attributes_key(&event.data.parent_block_hash, event.data.proposal_slot);
        if self.known_payload_attributes.contains_key(&payload_attributes_key) {
            return;
        }

        // Clean up old payload attributes
        self.known_payload_attributes.retain(|_, value| value.data.proposal_slot >= self.head_slot);

        // Save new one
        self.known_payload_attributes.insert(payload_attributes_key, event.clone());

        info!(
            head_slot = self.head_slot,
            payload_attribute_slot =% event.data.proposal_slot,
            payload_attribute_parent = ?event.data.parent_block_hash,
            "Processing payload attribute event",
        );

        // FIXME(alloy)
        let withdrawals_list: Withdrawals =
            event.data.payload_attributes.withdrawals.clone().into();
        let withdrawals_root = withdrawals_list.tree_hash_root();

        let update = ChainUpdate::PayloadAttributesUpdate(PayloadAttributesUpdate {
            slot: event.data.proposal_slot.as_u64(),
            parent_hash: event.data.parent_block_hash,
            withdrawals_root,
            payload_attributes: event.data.payload_attributes,
        });

        let _ = self.chain_update_tx.send(update);
    }
}
