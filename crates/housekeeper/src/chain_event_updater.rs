use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_primitives::B256;
use futures::{future, FutureExt};
use helix_beacon::types::{HeadEventData, PayloadAttributes, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, bid_sorter::BidSorterMessage,
    chain_info::ChainInfo, utils::utcnow_sec,
};
use helix_datastore::Auctioneer;
use helix_types::{Slot, SlotClockTrait};
use tokio::{
    sync::broadcast,
    time::{interval, interval_at, sleep, timeout, Instant},
};
use tracing::{error, info, warn};
use tree_hash::TreeHash;

use crate::CurrentSlotInfo;

// Do not accept slots more than 60 seconds in the future
const MAX_DISTANCE_FOR_FUTURE_SLOT: u64 = 60;
const CUTOFF_TIME: u64 = 4;

/// Payload for a new payload attribute event sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct PayloadAttributesUpdate {
    pub slot: Slot,
    pub parent_hash: B256,
    pub withdrawals_root: B256,
    pub payload_attributes: PayloadAttributes,
}

/// Payload for head event updates sent to subscribers.
#[derive(Clone, Debug, Default)]
pub struct SlotUpdate {
    pub slot: Slot,
    pub next_duty: Option<BuilderGetValidatorsResponseEntry>,
    pub new_duties: Option<Vec<BuilderGetValidatorsResponseEntry>>,
}

/// Manages the update of head slots and the fetching of new proposer duties.
pub struct ChainEventUpdater<A: Auctioneer + 'static> {
    head_slot: u64,
    known_payload_attributes: HashMap<(B256, Slot), PayloadAttributesEvent>,

    proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,

    auctioneer: Arc<A>,
    chain_info: Arc<ChainInfo>,
    curr_slot_info: CurrentSlotInfo,

    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
}

impl<A: Auctioneer + 'static> ChainEventUpdater<A> {
    pub fn new(
        auctioneer: Arc<A>,
        chain_info: Arc<ChainInfo>,
        curr_slot_info: CurrentSlotInfo,
        sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    ) -> Self {
        Self {
            head_slot: 0,
            known_payload_attributes: Default::default(),
            auctioneer,
            proposer_duties: Vec::new(),
            chain_info,
            curr_slot_info,
            sorter_tx,
        }
    }

    pub async fn start(
        mut self,
        head_event_rx: broadcast::Receiver<HeadEventData>,
        payload_attributes_rx: broadcast::Receiver<PayloadAttributesEvent>,
    ) {
        loop {
            let result = std::panic::AssertUnwindSafe(
                self.run(head_event_rx.resubscribe(), payload_attributes_rx.resubscribe()),
            )
            .catch_unwind()
            .await;

            match result {
                Ok(()) => {
                    info!("ChainEventUpdater exited normally");
                    break;
                }
                Err(err) => {
                    error!("ChainEventUpdater panicked: {:?}", err);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }

    /// Starts the updater and listens to head events and new subscriptions.
    pub async fn run(
        &mut self,
        mut head_event_rx: broadcast::Receiver<HeadEventData>,
        mut payload_attributes_rx: broadcast::Receiver<PayloadAttributesEvent>,
    ) {
        let start_instant = Instant::now() +
            self.chain_info.clock.duration_to_next_slot().unwrap() +
            Duration::from_secs(CUTOFF_TIME);
        let mut timer =
            interval_at(start_instant, Duration::from_secs(self.chain_info.seconds_per_slot()));

        let mut head_event_redis_rx = self.auctioneer.get_head_event();
        let mut payload_attributes_redis_rx = self.auctioneer.get_payload_attributes();

        // Watchdog to confirm the loop is alive
        let mut watchdog = interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                head_event_result = head_event_rx.recv() => {
                    match head_event_result {
                        Ok(head_event) => {
                            info!(head_slot =% head_event.slot, "Received head event from beacon node");
                            let publish_event = head_event.clone();
                            let this = self.auctioneer.clone();
                            tokio::spawn(async move {
                                if let Err(err) = this.publish_head_event(&publish_event).await {
                                    error!(%err, "Failed to publish head event");
                                }
                            });

                            self.process_slot(head_event.slot.into()).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("head events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("head event channel closed, resubscribing...");
                            head_event_rx = head_event_rx.resubscribe();
                        }
                    }
                }

                head_event_redis_result = head_event_redis_rx.recv() => {
                    match head_event_redis_result {
                        Ok(head_event) => {
                            info!(head_slot =% head_event.slot, "Received head event from Redis");
                            self.process_slot(head_event.slot.into()).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("head redis events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("head redis channel closed, resubscribing...");
                            head_event_redis_rx = self.auctioneer.get_head_event();
                        }
                    }
                }

                payload_attributes_result = payload_attributes_rx.recv() => {
                    match payload_attributes_result {
                        Ok(payload_attributes) => {
                            let publish_event = payload_attributes.clone();
                            let this = self.auctioneer.clone();
                            tokio::spawn(async move {
                                if let Err(err) = this.publish_payload_attributes(&publish_event).await {
                                    error!(%err, "Failed to publish payload attributes");
                                }
                            });

                            self.process_payload_attributes(payload_attributes).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("payload attributes events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("payload attributes channel closed, resubscribing...");
                            payload_attributes_rx = payload_attributes_rx.resubscribe();
                        }
                    }
                }

                payload_attributes_redis_result = payload_attributes_redis_rx.recv() => {
                    match payload_attributes_redis_result {
                        Ok(payload_attributes) => {
                            self.process_payload_attributes(payload_attributes).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("payload attributes redis events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("payload attributes redis channel closed, resubscribing...");
                            payload_attributes_redis_rx = self.auctioneer.get_payload_attributes();
                        }
                    }
                }

                _ = timer.tick() => {
                    info!("4 seconds into slot. Attempting slot update...");
                    match self.chain_info.clock.now() {
                        Some(slot) => self.process_slot(slot.as_u64()).await,
                        None => error!("could not get current slot"),
                    }
                }

                _ = watchdog.tick() => {
                    info!("ChainEventUpdater alive, head_slot = {}", self.head_slot);
                }
            }
        }
    }

    /// Handles a new slot in a non-blocking, timeout-safe way.
    async fn process_slot(&mut self, slot: u64) {
        if self.head_slot >= slot {
            return;
        }

        info!(head_slot =% slot, "Processing slot");

        // Validate this isn't a faulty head slot
        let slot_timestamp =
            self.chain_info.genesis_time_in_secs + (slot * self.chain_info.seconds_per_slot());
        if slot_timestamp > utcnow_sec() + MAX_DISTANCE_FOR_FUTURE_SLOT {
            warn!(head_slot = slot, "slot is too far in the future");
            return;
        }

        // Log missed slots
        if self.head_slot != 0 {
            for s in (self.head_slot + 1)..slot {
                warn!(missed_slot = s, "missed slot");
            }
        }

        self.head_slot = slot;

        sleep(Duration::from_secs(1)).await;

        let new_duties =
            match timeout(Duration::from_secs(1), self.auctioneer.get_proposer_duties()).await {
                Ok(Ok(mut new_duties)) => {
                    info!(
                        head_slot = slot,
                        new_duties = new_duties.len(),
                        "Fetched proposer duties from auctioneer"
                    );

                    let auctioneer = self.auctioneer.clone();
                    let futures = new_duties.iter_mut().map(|duty| {
                        let auctioneer = auctioneer.clone();
                        let pubkey = duty.entry.registration.message.pubkey.clone();
                        async move {
                            match timeout(
                                Duration::from_secs(1),
                                auctioneer.is_primev_proposer(&pubkey),
                            )
                            .await
                            {
                                Ok(Ok(true)) => {
                                    Some(duty.entry.registration.message.pubkey.clone())
                                }
                                Ok(Ok(false)) => None,
                                Ok(Err(err)) => {
                                    error!(%err, "Failed to check if proposer is primev");
                                    None
                                }
                                Err(_) => {
                                    error!("Timeout checking if proposer is primev");
                                    None
                                }
                            }
                        }
                    });

                    let results: Vec<_> = future::join_all(futures).await;

                    // Update duties with Primev builder if needed
                    for (duty, is_primev) in new_duties.iter_mut().zip(results.into_iter()) {
                        if is_primev.is_some() {
                            info!(head_slot = slot, "Primev proposer duty found");
                            match &mut duty.entry.preferences.trusted_builders {
                                Some(trusted_builders) => {
                                    trusted_builders.push("PrimevBuilder".to_string());
                                }
                                None => {
                                    duty.entry.preferences.trusted_builders =
                                        Some(vec!["PrimevBuilder".to_string()]);
                                }
                            }
                        }
                    }

                    self.proposer_duties.clone_from(&new_duties);
                    Some(new_duties)
                }
                Ok(Err(err)) => {
                    error!(%err, "Failed to get proposer duties from auctioneer");
                    None
                }
                Err(_) => {
                    error!("Timeout fetching proposer duties");
                    None
                }
            };

        // Get the next proposer duty for the new slot.
        let next_duty = self.proposer_duties.iter().find(|duty| duty.slot == slot + 1).cloned();

        let update = SlotUpdate { slot: slot.into(), new_duties, next_duty };

        // Run handle_new_slot in a spawned task so it can't block the select loop.
        let curr_slot_info = self.curr_slot_info.clone();
        let chain_info = self.chain_info.clone();
        tokio::spawn(async move {
            curr_slot_info.handle_new_slot(update, &chain_info);
        });
        let _ = self.sorter_tx.try_send(BidSorterMessage::Slot(slot));
    }

    // Handles a new payload attributes event
    async fn process_payload_attributes(&mut self, event: PayloadAttributesEvent) {
        // require new proposal slot in the future
        if self.head_slot >= event.data.proposal_slot.as_u64() {
            return;
        }

        // Discard payload attributes if already known
        let payload_attributes_key = &(event.data.parent_block_hash, event.data.proposal_slot);
        if self.known_payload_attributes.contains_key(payload_attributes_key) {
            return;
        }

        // Clean up old payload attributes
        self.known_payload_attributes.retain(|_, value| value.data.proposal_slot >= self.head_slot);

        // Save new one
        self.known_payload_attributes.insert(*payload_attributes_key, event.clone());

        info!(
            head_slot = self.head_slot,
            payload_attribute_slot =% event.data.proposal_slot,
            payload_attribute_parent = ?event.data.parent_block_hash,
            "Processing payload attribute event",
        );

        let withdrawals_root = event.data.payload_attributes.withdrawals.tree_hash_root();

        let update = PayloadAttributesUpdate {
            slot: event.data.proposal_slot,
            parent_hash: event.data.parent_block_hash,
            withdrawals_root,
            payload_attributes: event.data.payload_attributes,
        };

        self.curr_slot_info.handle_new_payload_attributes(update);
    }
}
