use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ethereum_consensus::{
    configs::{goerli::CAPELLA_FORK_EPOCH, mainnet::SECONDS_PER_SLOT},
    deneb::Withdrawal,
    primitives::Bytes32,
};
use tokio::{
    sync::{broadcast, mpsc},
    time::{interval_at, sleep, Instant},
};
use tracing::{error, info, warn};

use helix_beacon_client::types::{HeadEventData, PayloadAttributes, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry,
    bellatrix::{List, Merkleized, Node},
    chain_info::ChainInfo,
};
use helix_database::DatabaseService;
use helix_utils::{get_payload_attributes_key, has_reached_fork};

// Do not accept slots more than 60 seconds in the future
const MAX_DISTANCE_FOR_FUTURE_SLOT: u64 = 60;
const CUTT_OFF_TIME: u64 = 4;

/// Payload for a new payload attribute event sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct PayloadAttributesUpdate {
    pub slot: u64,
    pub parent_hash: Bytes32,
    pub withdrawals_root: Option<Node>,
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

    head_slot: u64,
    known_payload_attributes: HashMap<String, PayloadAttributesEvent>,

    proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,

    database: Arc<D>,
    subscription_channel: mpsc::Receiver<mpsc::Sender<ChainUpdate>>,
    chain_info: Arc<ChainInfo>,
}

impl<D: DatabaseService> ChainEventUpdater<D> {
    pub fn new_with_channel(
        database: Arc<D>,
        subscription_channel: mpsc::Receiver<mpsc::Sender<ChainUpdate>>,
        chain_info: Arc<ChainInfo>,
    ) -> Self {
        Self {
            subscribers: Vec::new(),
            head_slot: 0,
            known_payload_attributes: Default::default(),
            database,
            subscription_channel,
            proposer_duties: Vec::new(),
            chain_info,
        }
    }

    pub fn new(
        database: Arc<D>,
        chain_info: Arc<ChainInfo>,
    ) -> (Self, mpsc::Sender<mpsc::Sender<ChainUpdate>>) {
        let (tx, rx) = mpsc::channel(200);
        let updater = Self::new_with_channel(database, rx, chain_info);
        (updater, tx)
    }

    /// Starts the updater and listens to head events and new subscriptions.
    pub async fn start(
        &mut self,
        mut head_event_rx: broadcast::Receiver<HeadEventData>,
        mut payload_attributes_rx: broadcast::Receiver<PayloadAttributesEvent>,
    ) {
        let start_instant = Instant::now() +
            self.chain_info.clock.duration_until_next_slot() +
            Duration::from_secs(CUTT_OFF_TIME);
        let mut timer =
            interval_at(start_instant, Duration::from_secs(self.chain_info.seconds_per_slot));
        loop {
            tokio::select! {
                head_event_result = head_event_rx.recv() => {
                    match head_event_result {
                        Ok(head_event) => {
                            info!(head_slot = head_event.slot, "Received head event");
                            self.process_slot(head_event.slot).await
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
                Some(sender) = self.subscription_channel.recv() => {
                    self.subscribers.push(sender);
                }
                _ = timer.tick() => {
                    info!("4 seconds into slot. Attempting to slot update...");
                    match self.chain_info.clock.current_slot() {
                        Some(slot) => self.process_slot(slot).await,
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
            return
        }

        info!(head_slot = slot, "Processing slot",);

        // Validate this isn't a faulty head slot
        if let Ok(current_timestamp) = SystemTime::now().duration_since(UNIX_EPOCH) {
            let slot_timestamp = self.chain_info.genesis_time_in_secs + (slot * SECONDS_PER_SLOT);
            if slot_timestamp > current_timestamp.as_secs() + MAX_DISTANCE_FOR_FUTURE_SLOT {
                warn!(head_slot = slot, "slot is too far in the future",);
                return
            }
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
        let new_duties = match self.database.get_proposer_duties().await {
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
        if let Some(new_duties) = &new_duties {
            self.proposer_duties.clone_from(new_duties);
        }

        // Get the next proposer duty for the new slot.
        let next_duty = self.proposer_duties.iter().find(|duty| duty.slot == slot + 1).cloned();

        let update = ChainUpdate::SlotUpdate(SlotUpdate { slot, new_duties, next_duty });
        self.send_update_to_subscribers(update).await;
    }

    // Handles a new payload attributes event
    async fn process_payload_attributes(&mut self, event: PayloadAttributesEvent) {
        // require new proposal slot in the future
        if self.head_slot >= event.data.proposal_slot {
            return
        }

        // Discard payload attributes if already known
        let payload_attributes_key =
            get_payload_attributes_key(&event.data.parent_block_hash, event.data.proposal_slot);
        if self.known_payload_attributes.contains_key(&payload_attributes_key) {
            return
        }

        // Clean up old payload attributes
        self.known_payload_attributes.retain(|_, value| value.data.proposal_slot >= self.head_slot);

        // Save new one
        self.known_payload_attributes.insert(payload_attributes_key, event.clone());

        info!(
            head_slot = self.head_slot,
            payload_attribute_slot = event.data.proposal_slot,
            payload_attribute_parent = ?event.data.parent_block_hash,
            "Processing payload attribute event",
        );

        let mut withdrawals_root = None;
        if has_reached_fork(event.data.proposal_slot, CAPELLA_FORK_EPOCH) {
            let mut withdrawals_list: List<Withdrawal, 16> =
                event.data.payload_attributes.withdrawals.clone().try_into().unwrap();
            withdrawals_root = withdrawals_list.hash_tree_root().ok();
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
}
