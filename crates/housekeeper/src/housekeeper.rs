use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use alloy_eips::merge::EPOCH_SLOTS;
use alloy_primitives::{map::HashSet, U256};
use futures::future::join_all;
use helix_beacon::{
    error::BeaconClientError,
    multi_beacon_client::MultiBeaconClient,
    types::{HeadEventData, StateId},
};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo,
    pending_block::PendingBlock, task, utils::utcnow_ms, BuilderInfo, ProposerDuty, RelayConfig,
    SignedValidatorRegistrationEntry,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{BlsPublicKey, Epoch, Slot, SlotClockTrait};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn, Instrument};
use uuid::Uuid;

use crate::{error::HousekeeperError, EthereumPrimevService};

const PROPOSER_DUTIES_UPDATE_FREQ: u64 = 1;

const TRUSTED_PROPOSERS_UPDATE_FREQ: u64 = 5;

const CUTOFF_TIME: Duration = Duration::from_secs(4);

// Constants for known validators refresh logic.
const MIN_SLOTS_BETWEEN_UPDATES: u64 = 6;
const MAX_SLOTS_BEFORE_FORCED_UPDATE: u64 = 32;

// Max time between header and payload for OptimsiticV2 submissions
const MAX_DELAY_BETWEEN_V2_SUBMISSIONS_MS: u64 = 2_000;
/// Max time to wait for payload after header is received for OptimsiticV2 submissions. We leave
/// more time here to account for some overhead/processing time. Note This should be
/// [`MAX_DELAY_BETWEEN_V2_SUBMISSIONS_MS`] + max overhead, but we leave some buffer. Note this also
/// works because the keepalive of pending blocks in redis is longer than this
const MAX_DELAY_WITH_NO_V2_PAYLOAD_MS: u64 = 20_000;

#[derive(Default, Clone)]
struct HousekeeperSlots {
    head: Arc<AtomicU64>,
    proposer_duties: Arc<AtomicU64>,
    refreshed_validators: Arc<AtomicU64>,
    trusted_proposers: Arc<AtomicU64>,
}

impl HousekeeperSlots {
    fn head_already_seen(&self, new_slot: Slot) -> bool {
        self.head.fetch_max(new_slot.as_u64(), Ordering::Relaxed) >= new_slot.as_u64()
    }
    fn proposer_duties(&self) -> u64 {
        self.proposer_duties.load(Ordering::Relaxed)
    }
    fn update_proposer_duties(&self, head_slot: Slot) {
        self.proposer_duties.store(head_slot.as_u64(), Ordering::Relaxed);
    }
    fn refreshed_validators(&self) -> u64 {
        self.refreshed_validators.load(Ordering::Relaxed)
    }
    fn update_refreshed_validators(&self, head_slot: Slot) {
        self.refreshed_validators.store(head_slot.as_u64(), Ordering::Relaxed);
    }
    fn trusted_proposers(&self) -> u64 {
        self.trusted_proposers.load(Ordering::Relaxed)
    }
    fn update_trusted_proposers(&self, head_slot: Slot) {
        self.trusted_proposers.store(head_slot.as_u64(), Ordering::Relaxed);
    }
}

/// Housekeeper Service.
///
/// Responsible for updating and managing known validators and proposer duties.
/// Also responsible for keeping the Auctioneer builder info up to date after manual changes.
/// If running multiple API instances in a single region only one housekeeper is needed as services
/// will sync through db.
#[derive(Clone)]
pub struct Housekeeper<DB: DatabaseService + 'static, A: Auctioneer + 'static> {
    db: Arc<DB>,
    beacon_client: Arc<MultiBeaconClient>,
    auctioneer: Arc<A>,
    chain_info: Arc<ChainInfo>,
    leader_id: Arc<String>,
    primev_service: Option<EthereumPrimevService>,
    slots: HousekeeperSlots,
}

impl<DB: DatabaseService, A: Auctioneer> Housekeeper<DB, A> {
    pub fn new(
        db: Arc<DB>,
        beacon_client: Arc<MultiBeaconClient>,
        auctioneer: Arc<A>,
        config: &RelayConfig,
        chain_info: Arc<ChainInfo>,
    ) -> Self {
        let primev_service =
            config.primev_config.clone().map(|p| EthereumPrimevService::new(p).unwrap());

        Self {
            db,
            beacon_client,
            auctioneer,
            chain_info,
            leader_id: Uuid::new_v4().to_string().into(),
            primev_service,
            slots: HousekeeperSlots::default(),
        }
    }

    // if this fails, all beacon nodes are down
    pub async fn start(
        self,
        head_event_rx: broadcast::Receiver<HeadEventData>,
    ) -> Result<(), BeaconClientError> {
        let best_sync_status = self.beacon_client.best_sync_status().await?;
        self.process_new_slot(best_sync_status.head_slot).await;

        tokio::spawn(self.run(head_event_rx));

        Ok(())
    }

    async fn run(self, mut head_event_rx: broadcast::Receiver<HeadEventData>) {
        loop {
            let sleep_time = self.chain_info.clock.duration_to_next_slot().unwrap() + CUTOFF_TIME;

            tokio::select! {
                head_event_result = head_event_rx.recv() => {
                    match head_event_result {
                        Ok(head_event) => {
                            self.process_new_slot(head_event.slot).await;
                        }
                        Err(err) => {
                            error!(%err, "failed to receive head event");
                        }
                    }
                }

                _ = tokio::time::sleep(sleep_time) => {
                    let head_slot = self.chain_info.current_slot();
                    self.process_new_slot(head_slot).await;
                }
            }
        }
    }

    #[tracing::instrument(skip(self))]
    async fn process_new_slot(&self, head_slot: Slot) {
        if self.slots.head_already_seen(head_slot) {
            return;
        }

        // only allow one housekeeper task to run at a time.
        if !self.auctioneer.try_acquire_or_renew_leadership(&self.leader_id).await {
            return;
        }

        let epoch = head_slot.epoch(self.chain_info.slots_per_epoch());
        info!(
            slot_pos = self.chain_info.slot_in_epoch(head_slot),
            %epoch,
            epoch_start = %epoch.start_slot(self.chain_info.slots_per_epoch()),
            epoch_end = %epoch.end_slot(self.chain_info.slots_per_epoch()),
            "processing new slot",
        );

        let mut tasks = Vec::new();

        // v2 demotions checks every slot
        let housekeeper = self.clone();
        tasks.push(task::spawn(
            file!(),
            line!(),
            async move {
                let start = Instant::now();
                if let Err(err) = housekeeper.demote_builders_with_expired_pending_blocks().await {
                    error!(%err, "failed to demote builders");
                }
                info!(duration = ?start.elapsed(), "demote builders task completed");
            }
            .in_current_span(),
        ));

        // proposer duties
        if self.should_update_duties(head_slot) {
            let housekeeper = self.clone();
            tasks.push(task::spawn(
                file!(),
                line!(),
                async move {
                    let start = Instant::now();
                    match housekeeper.update_proposer_duties(epoch).await {
                        Ok(proposer_duties) => {
                            let slots = housekeeper.slots.clone();
                            // don't wait for primev update to complete
                            tokio::spawn(
                                async move {
                                    if let Err(err) = housekeeper
                                        .maybe_primev_update_with_duties(proposer_duties)
                                        .await
                                    {
                                        error!(%err, "failed to update primev duties");
                                    }
                                }
                                .in_current_span(),
                            );

                            slots.update_proposer_duties(head_slot);
                            info!(duration = ?start.elapsed(), "proposer duties task completed");
                        }

                        Err(err) => {
                            error!(%err, "failed to update proposer duties");
                        }
                    }
                }
                .in_current_span(),
            ));
        };

        // known validators
        if self.should_refresh_known_validators(head_slot.as_u64()) {
            let housekeeper = self.clone();
            tasks.push(task::spawn(
                file!(),
                line!(),
                async move {
                    let start = Instant::now();
                    if let Err(err) = housekeeper.refresh_known_validators().await {
                        error!(%err, "failed to refresh known validators");
                    } else {
                        housekeeper.slots.update_refreshed_validators(head_slot);
                    }
                    info!(duration = ?start.elapsed(), "refresh validators task completed");
                }
                .in_current_span(),
            ));
        }

        // builder info updated every slot
        let housekeeper = self.clone();
        tasks.push(task::spawn(
            file!(),
            line!(),
            async move {
                let start = Instant::now();
                if let Err(err) = housekeeper.sync_builder_info_changes().await {
                    error!(%err, "failed to sync builder info changes");
                }
                info!(duration = ?start.elapsed(), "sync builder info task completed");
            }
            .in_current_span(),
        ));

        // trusted proposers
        if self.should_update_trusted_proposers(head_slot.as_u64()) {
            let housekeeper = self.clone();
            task::spawn(
                file!(),
                line!(),
                async move {
                    let start = Instant::now();
                    if let Err(err) = housekeeper.update_trusted_proposers().await {
                        error!(%err, "failed to update trusted proposers");
                    } else {
                        housekeeper.slots.update_trusted_proposers(head_slot);
                    }
                    info!(duration = ?start.elapsed(), "update trusted proposers task completed");
                }
                .in_current_span(),
            );
        }

        // wait for all tasks, this should take less than one slot
        join_all(tasks).await;
    }

    /// Refresh the list of known validators by querying the beacon client.
    /// Refreshed validators are stored in the database.
    /// This is potentially very slow as it fetches all active validators (~1mm entries in mainnet).
    async fn refresh_known_validators(&self) -> Result<(), HousekeeperError> {
        let start = Instant::now();
        let validators = self.beacon_client.get_state_validators(StateId::Head).await?;
        debug!(validators = validators.len(), duration = ?start.elapsed(), "fetched validators");

        self.db.set_known_validators(validators).await?;

        Ok(())
    }

    /// Synchronizes builder information changes.
    async fn sync_builder_info_changes(&self) -> Result<(), HousekeeperError> {
        let builder_infos = self.db.get_all_builder_infos().await?;
        debug!(builder_infos = builder_infos.len(), "updating builder infos");
        self.auctioneer.update_builder_infos(builder_infos).await?;

        Ok(())
    }

    /// Handle valid payload Optimistic V2 demotions.
    ///
    /// There are two cases where we might demote a builder here.
    /// 1) They sent a header but we received no accompanying payload.
    /// 2) The payload was received > 2 seconds after we received the header.
    ///
    /// DB entries are also removed if they have been waiting for over 45 seconds.
    // TODO: here we're most likely processing the same pending blocks several times
    async fn demote_builders_with_expired_pending_blocks(&self) -> Result<(), HousekeeperError> {
        let mut demoted_builders = HashSet::new();
        let pending_blocks = self.auctioneer.get_pending_blocks().await?;

        debug!(pending_blocks = pending_blocks.len(), "checking expired pending blocks");

        for pending_block in pending_blocks {
            if demoted_builders.contains(&pending_block.builder_pubkey) {
                continue;
            }

            if v2_submission_late(&pending_block) {
                let reason =
                    format!("builder demoted due to missing payload submission. {pending_block:?}");
                info!(builder_pubkey = ?pending_block.builder_pubkey, reason);
                self.auctioneer.demote_builder(&pending_block.builder_pubkey).await?;
                self.db
                    .db_demote_builder(
                        &pending_block.builder_pubkey,
                        &pending_block.block_hash,
                        reason,
                    )
                    .await?;

                demoted_builders.insert(pending_block.builder_pubkey);
            }
        }

        Ok(())
    }

    /// Determine if known validators should be refreshed for the given slot.
    fn should_refresh_known_validators(&self, head_slot: u64) -> bool {
        let last_updated = self.slots.refreshed_validators();
        if head_slot <= last_updated {
            return false;
        }

        let slots_since_last_update = head_slot - last_updated;
        if slots_since_last_update < MIN_SLOTS_BETWEEN_UPDATES {
            return false;
        }

        let force_update = slots_since_last_update > MAX_SLOTS_BEFORE_FORCED_UPDATE;
        if force_update {
            return true;
        }

        let head_slot_pos = (head_slot % EPOCH_SLOTS) + 1; // position in epoch.
        head_slot_pos == 4 || head_slot_pos == 20
    }

    /// Update proposer duties for `head_slot` and `head_slot` + 1.
    /// Returns the fetched proposer duties on success.
    async fn update_proposer_duties(
        &self,
        epoch: Epoch,
    ) -> Result<Vec<ProposerDuty>, HousekeeperError> {
        debug!(%epoch, "updating proposer duties");

        let proposer_duties = self.fetch_duties(epoch.as_u64()).await?;

        // Check if signed validator registrations exist for each proposer duty
        let pubkeys: Vec<&BlsPublicKey> =
            proposer_duties.iter().map(|duty| &duty.public_key).collect();
        let signed_validator_registrations =
            self.fetch_signed_validator_registrations(pubkeys.as_slice()).await?;

        // Format duties and save to the database
        if signed_validator_registrations.is_empty() {
            warn!(%epoch, "no registrationts found");
        } else {
            let mut formatted_proposer_duties = Vec::with_capacity(proposer_duties.len());

            for duty in proposer_duties.iter() {
                if let Some(reg) = signed_validator_registrations.get(&duty.public_key) {
                    formatted_proposer_duties.push(BuilderGetValidatorsResponseEntry {
                        slot: duty.slot.into(),
                        validator_index: duty.validator_index,
                        entry: reg.registration_info.clone(),
                    });
                }
            }

            info!(
                duties = formatted_proposer_duties.capacity(),
                registered = formatted_proposer_duties.len(),
                "storing proposer duties"
            );

            self.db.set_proposer_duties(formatted_proposer_duties).await?;
        }

        Ok(proposer_duties)
    }

    fn should_update_duties(&self, head_slot: Slot) -> bool {
        let last_updated = self.slots.proposer_duties();
        head_slot.as_u64().saturating_sub(last_updated) >= PROPOSER_DUTIES_UPDATE_FREQ
    }

    /// Updates primev builders and validators using pre-fetched proposer duties
    async fn maybe_primev_update_with_duties(
        &self,
        proposer_duties: Vec<ProposerDuty>,
    ) -> Result<(), HousekeeperError> {
        let Some(primev_service) = self.primev_service.as_ref() else {
            return Ok(());
        };

        let primev_builders = primev_service.get_registered_primev_builders().await;

        for builder_pubkey in primev_builders {
            self.db
                .store_builder_info(&builder_pubkey, &BuilderInfo {
                    collateral: U256::ZERO,
                    is_optimistic: false,
                    is_optimistic_for_regional_filtering: false,
                    builder_id: Some("PrimevBuilder".to_string()),
                    builder_ids: Some(vec!["PrimevBuilder".to_string()]),
                })
                .await?;
        }

        let primev_validators =
            primev_service.get_registered_primev_validators(proposer_duties).await;
        self.auctioneer.update_primev_proposers(&primev_validators).await?;

        Ok(())
    }

    fn should_update_trusted_proposers(&self, head_slot: u64) -> bool {
        let last_updated = self.slots.trusted_proposers();
        head_slot % TRUSTED_PROPOSERS_UPDATE_FREQ == 0 ||
            head_slot.saturating_sub(last_updated) >= TRUSTED_PROPOSERS_UPDATE_FREQ
    }

    /// Update the proposer whitelist.
    ///
    /// This function will fetch the proposer whitelist from the database and update the auctioneer.
    /// It will also update the `refreshed_trusted_proposers_slot` to the current `head_slot`.
    async fn update_trusted_proposers(&self) -> Result<(), HousekeeperError> {
        let start = Instant::now();
        let proposer_whitelist = self.db.get_trusted_proposers().await?;
        debug!(proposer_whitelist = proposer_whitelist.len(), duration = ?start.elapsed(), "fetched trusted proposers");
        self.auctioneer.update_trusted_proposers(proposer_whitelist).await?;

        Ok(())
    }

    async fn fetch_duties(&self, epoch: u64) -> Result<Vec<ProposerDuty>, BeaconClientError> {
        let (_, mut proposer_duties) = self.beacon_client.get_proposer_duties(epoch).await?;

        match self.beacon_client.get_proposer_duties(epoch + 1).await {
            Ok((_, mut next_duties)) => proposer_duties.append(&mut next_duties),
            Err(err) => error!(epoch = epoch + 1, %err, "failed fetching next proposer duties"),
        }

        Ok(proposer_duties)
    }

    /// Fetch validator registrations for `pub_keys` from database.
    async fn fetch_signed_validator_registrations(
        &self,
        pubkeys: &[&BlsPublicKey],
    ) -> Result<HashMap<BlsPublicKey, SignedValidatorRegistrationEntry>, HousekeeperError> {
        let registrations: Vec<SignedValidatorRegistrationEntry> =
            self.db.get_validator_registrations_for_pub_keys(pubkeys).await?;

        let registrations =
            registrations.into_iter().map(|entry| (entry.public_key().clone(), entry)).collect();

        Ok(registrations)
    }
}

/// Calculates the delay in submission of the payload after a header.
///
/// Returns true if the payload was received over 2 seconds after the header or if the payload was
/// never received. Otherwise, returns false.
fn v2_submission_late(pending_block: &PendingBlock) -> bool {
    match (pending_block.header_receive_ms, pending_block.payload_receive_ms) {
        // no header
        (None, _) => false,
        // no payload yet
        (Some(header_receive_ms), None) => {
            (utcnow_ms().saturating_sub(header_receive_ms)) > MAX_DELAY_WITH_NO_V2_PAYLOAD_MS
        }
        // payload received
        (Some(header_receive_ms), Some(payload_receive_ms)) => {
            payload_receive_ms.saturating_sub(header_receive_ms) >
                MAX_DELAY_BETWEEN_V2_SUBMISSIONS_MS
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_dont_process_same_slot() {
        let slots = HousekeeperSlots::default();
        assert!(!slots.head_already_seen(10u64.into()));
        assert!(slots.head_already_seen(9u64.into()));
        assert!(slots.head_already_seen(10u64.into()));
        assert!(!slots.head_already_seen(11u64.into()));
    }
}
