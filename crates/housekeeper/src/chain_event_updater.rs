use std::sync::Arc;

use ethereum_consensus::{configs::goerli::CAPELLA_FORK_EPOCH, primitives::Bytes32};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use helix_beacon_client::{
    types::{HeadEventData, PayloadAttributes, PayloadAttributesEvent},
    MultiBeaconClientTrait,
};
use helix_database::DatabaseService;
use helix_common::api::builder_api::BuilderGetValidatorsResponseEntry;
use helix_utils::{calculate_withdrawals_root, has_reached_fork};

/// Payload for a new payload attribute event sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct PayloadAttributesUpdate {
    pub slot: u64,
    pub parent_hash: Bytes32,
    pub withdrawals_root: Option<[u8; 32]>,
    pub payload_attributes: PayloadAttributes,
}

/// Payload for head event updates sent to subscribers.
#[derive(Clone, Debug)]
pub struct SlotUpdate {
    pub slot: u64,
    pub next_duty: Option<BuilderGetValidatorsResponseEntry>,
    pub new_duties: Option<Vec<BuilderGetValidatorsResponseEntry>>,
}

#[derive(Clone, Debug)]
pub enum ChainUpdate {
    SlotUpdate(SlotUpdate),
    PayloadAttributesUpdate(PayloadAttributesUpdate),
}

/// Manages the update of head slots and the fetching of new proposer duties.
pub struct ChainEventUpdater<D: DatabaseService> {
    subscribers: Vec<mpsc::Sender<ChainUpdate>>,

    last_slot: u64,
    last_payload_attribute_parent: Bytes32,

    proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,

    database: Arc<D>,
    subscription_channel: mpsc::Receiver<mpsc::Sender<ChainUpdate>>,
}

impl<D: DatabaseService> ChainEventUpdater<D> {
    pub fn new_with_channel(
        database: Arc<D>,
        subscription_channel: mpsc::Receiver<mpsc::Sender<ChainUpdate>>,
    ) -> Self {
        Self {
            subscribers: Vec::new(),
            last_slot: 0,
            last_payload_attribute_parent: Bytes32::default(),
            database,
            subscription_channel,
            proposer_duties: Vec::new(),
        }
    }

    pub fn new(database: Arc<D>) -> (Self, mpsc::Sender<mpsc::Sender<ChainUpdate>>) {
        let (tx, rx) = mpsc::channel(200);
        let updater = Self::new(database, rx);
        (updater, tx)
    }

    /// Starts the updater and listens to head events and new subscriptions.
    pub async fn start<T: MultiBeaconClientTrait>(&mut self, beacon_client: T) {
        let mut head_event_rx = self.subscribe_to_head_events(&beacon_client).await;
        let mut payload_attributes_rx = self.subscribe_to_payload_attributes(&beacon_client).await;

        loop {
            tokio::select! {
                head_event_result = head_event_rx.recv() => {
                    match head_event_result {
                        Some(head_event) => self.process_head_event(head_event).await,
                        None => {
                            // Re-subscribe if the channel was closed
                            head_event_rx = self.subscribe_to_head_events(&beacon_client).await;
                        },
                    }
                }
                payload_attributes_result = payload_attributes_rx.recv() => {
                    match payload_attributes_result {
                        Some(payload_attributes) => self.process_payload_attributes(payload_attributes).await,
                        None => {
                            // Re-subscribe if the channel was closed
                            payload_attributes_rx = self.subscribe_to_payload_attributes(&beacon_client).await;
                        },
                    }
                }
                Some(sender) = self.subscription_channel.recv() => {
                    self.subscribers.push(sender);
                }
            }
        }
    }

    /// Handles a new head event.
    async fn process_head_event(&mut self, event: HeadEventData) {
        if self.last_slot >= event.slot {
            return;
        }

        info!(head_slot = event.slot, "Processing head event",);

        // Log any missed slots
        if self.last_slot != 0 {
            for s in (self.last_slot + 1)..event.slot {
                warn!(missed_slot = s, "missed slot");
            }
        }

        self.last_slot = event.slot;

        // Re-fetch proposer duties every 8 slots
        let new_duties = if event.slot % 8 == 0 {
            match self.database.get_proposer_duties().await {
                Ok(new_duties) => Some(new_duties),
                Err(err) => {
                    error!(error = %err, "Failed to get proposer duties from db");
                    None
                }
            }
        } else {
            None
        };

        // Update local cache if new duties were fetched.
        if let Some(new_duties) = &new_duties {
            self.proposer_duties = new_duties.clone();
        }

        // Get the next proposer duty for the new slot.
        let next_duty =
            self.proposer_duties.iter().find(|duty| duty.slot == event.slot + 1).cloned();

        let update =
            ChainUpdate::SlotUpdate(SlotUpdate { slot: event.slot, new_duties, next_duty });
        self.send_update_to_subscribers(update).await;
    }

    // Handles a new payload attributes event
    async fn process_payload_attributes(&mut self, event: PayloadAttributesEvent) {
        // require new proposal slot in the future
        if self.last_slot >= event.data.proposal_slot {
            return;
        }
        if self.last_payload_attribute_parent == event.data.parent_block_hash {
            return;
        }

        info!(
            head_slot = self.last_slot,
            payload_attribute_slot = event.data.proposal_slot,
            payload_attribute_parent = ?event.data.parent_block_hash,
            "Processing payload attribute event",
        );

        let mut withdrawals_root = None;
        if has_reached_fork(event.data.proposal_slot, CAPELLA_FORK_EPOCH) {
            withdrawals_root =
                Some(calculate_withdrawals_root(&event.data.payload_attributes.withdrawals));
        }

        let update = ChainUpdate::PayloadAttributesUpdate(PayloadAttributesUpdate {
            slot: event.data.proposal_slot,
            parent_hash: event.data.parent_block_hash,
            withdrawals_root,
            payload_attributes: event.data.payload_attributes,
        });

        self.send_update_to_subscribers(update).await;
    }

    async fn send_update_to_subscribers(&mut self, update: ChainUpdate) {
        // Store subscribers that should be unsubscribed
        let mut to_unsubscribe = Vec::new();

        // Send updates to all subscribers
        for (index, tx) in self.subscribers.iter().enumerate() {
            if let Err(err) = tx.send(update.clone()).await {
                error!(
                    index = index,
                    error = %err,
                    "Failed to send update to subscriber",
                );
                to_unsubscribe.push(index);
            }
        }

        // Unsubscribe any failed subscribers
        for index in to_unsubscribe.into_iter().rev() {
            self.subscribers.remove(index);
        }
    }

    /// Subscribe to head events from the beacon client.
    async fn subscribe_to_head_events<T: MultiBeaconClientTrait>(
        &self,
        beacon_client: &T,
    ) -> mpsc::Receiver<HeadEventData> {
        let (tx, rx) = mpsc::channel(100);
        beacon_client.subscribe_to_head_events(tx).await;
        rx
    }

    /// Subscribe to head events from the beacon client.
    async fn subscribe_to_payload_attributes<T: MultiBeaconClientTrait>(
        &self,
        beacon_client: &T,
    ) -> mpsc::Receiver<PayloadAttributesEvent> {
        let (tx, rx) = mpsc::channel(100);
        beacon_client.subscribe_to_payload_attributes_events(tx).await;
        rx
    }
}
