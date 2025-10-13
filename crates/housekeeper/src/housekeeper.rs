use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_eips::merge::EPOCH_SLOTS;
use alloy_primitives::{B256, U256};
use helix_beacon::{
    error::BeaconClientError,
    multi_beacon_client::MultiBeaconClient,
    types::{HeadEventData, StateId},
};
use helix_common::{
    BuilderConfig, BuilderInfo, ProposerDuty, RelayConfig, SignedValidatorRegistrationEntry,
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo, is_local_dev,
    local_cache::LocalCache, task, utils::utcnow_dur,
};
use helix_database::DatabaseService;
use helix_network::RelayNetworkManager;
use helix_types::{BlsPublicKeyBytes, Epoch, Slot, SlotClockTrait};
use tokio::sync::{Mutex, broadcast};
use tracing::{Instrument, debug, error, info, warn};

use crate::{
    EthereumPrimevService, chain_event_updater::SlotData, error::HousekeeperError,
    inclusion_list::InclusionListService,
};

const PROPOSER_DUTIES_UPDATE_FREQ: u64 = 1;

const TRUSTED_PROPOSERS_UPDATE_FREQ: u64 = 5;

const CUTOFF_TIME: Duration = Duration::from_secs(4);

// Constants for known validators refresh logic.
const MIN_SLOTS_BETWEEN_UPDATES: u64 = 6;
const MAX_SLOTS_BEFORE_FORCED_UPDATE: u64 = 32;

#[derive(Default, Clone)]
struct HousekeeperSlots {
    head: Arc<AtomicU64>,
    proposer_duties: Arc<AtomicU64>,
    refreshed_validators: Arc<AtomicU64>,
    trusted_proposers: Arc<AtomicU64>,
    // updating can take >> slot time, so we avoid concurrent updates by locking the mutex
    updating_builder_infos: Arc<Mutex<()>>,
    updating_proposer_duties: Arc<Mutex<()>>,
    updating_refreshed_validators: Arc<Mutex<()>>,
    updating_trusted_proposers: Arc<Mutex<()>>,
}

impl HousekeeperSlots {
    fn head(&self) -> Slot {
        self.head.load(Ordering::Relaxed).into()
    }
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
pub struct Housekeeper<DB: DatabaseService + 'static> {
    db: Arc<DB>,
    beacon_client: Arc<MultiBeaconClient>,
    auctioneer: Arc<LocalCache>,
    chain_info: Arc<ChainInfo>,
    primev_service: Option<EthereumPrimevService>,
    slots: HousekeeperSlots,
    inclusion_list_service: Option<InclusionListService<DB>>,
    local_builders: Vec<BuilderConfig>,
}

impl<DB: DatabaseService> Housekeeper<DB> {
    pub fn new(
        db: Arc<DB>,
        beacon_client: Arc<MultiBeaconClient>,
        auctioneer: Arc<LocalCache>,
        config: &RelayConfig,
        chain_info: Arc<ChainInfo>,
        auctioneer_handle: crossbeam_channel::Sender<SlotData>,
        relay_network_api: Arc<RelayNetworkManager>,
    ) -> Self {
        let primev_service =
            config.primev_config.clone().map(|p| EthereumPrimevService::new(p).unwrap());

        let inclusion_list_service = config.inclusion_list.clone().map(|config| {
            InclusionListService::new(
                db.clone(),
                auctioneer.clone(),
                config,
                chain_info.clone(),
                auctioneer_handle,
                relay_network_api,
            )
        });

        Self {
            db,
            beacon_client,
            auctioneer,
            chain_info,
            primev_service,
            slots: HousekeeperSlots::default(),
            inclusion_list_service,
            local_builders: config.builders.clone(),
        }
    }

    // if this fails, all beacon nodes are down
    pub async fn start(
        self,
        head_event_rx: broadcast::Receiver<HeadEventData>,
    ) -> Result<(), BeaconClientError> {
        let best_sync_status = self.beacon_client.best_sync_status().await?;
        self.process_new_slot(best_sync_status.head_slot, None).await;

        tokio::spawn(self.run(head_event_rx));

        Ok(())
    }

    async fn run(self, mut head_event_rx: broadcast::Receiver<HeadEventData>) {
        loop {
            let head = self.slots.head();
            let timeout = (self.chain_info.clock.start_of(head + 1).unwrap())
                .saturating_sub(utcnow_dur()) +
                CUTOFF_TIME;

            if let Ok(head_event_result) = tokio::time::timeout(timeout, head_event_rx.recv()).await
            {
                match head_event_result {
                    Ok(head_event) => {
                        self.process_new_slot(head_event.slot, Some(head_event.block)).await;
                    }
                    Err(err) => {
                        error!(%err, "failed to receive head event");
                    }
                }
            } else {
                self.process_new_slot(self.chain_info.current_slot(), None).await;
            }
        }
    }

    #[tracing::instrument(skip_all, name = "new_slot", fields(slot = %head_slot))]
    async fn process_new_slot(&self, head_slot: Slot, block_hash: Option<B256>) {
        if self.slots.head_already_seen(head_slot) {
            return;
        }

        let epoch = head_slot.epoch(self.chain_info.slots_per_epoch());
        info!(
            from_timeout = block_hash.is_none(),
            into_slot =? self.chain_info.duration_into_slot(head_slot).unwrap_or_default(),
            slot_pos = self.chain_info.slot_in_epoch(head_slot),
            %epoch,
            epoch_start = %epoch.start_slot(self.chain_info.slots_per_epoch()),
            epoch_end = %epoch.end_slot(self.chain_info.slots_per_epoch()),
            "processing new slot",
        );

        // proposer duties
        if self.should_update_duties(head_slot) {
            let housekeeper = self.clone();
            task::spawn(
                file!(),
                line!(),
                async move {
                    let Ok(_guard) = housekeeper.slots.updating_proposer_duties.try_lock() else {
                        warn!("proposer duties update already in progress");
                        return;
                    };
                    let start = Instant::now();

                    housekeeper.update_proposer_duties(head_slot, epoch, block_hash).await;

                    info!(duration = ?start.elapsed(), "proposer duties task completed");
                }
                .in_current_span(),
            );
        }

        // known validators
        if self.should_refresh_known_validators(head_slot.as_u64()) {
            if is_local_dev() {
                warn!("skipping refresh of known validators")
            } else {
                let housekeeper = self.clone();
                task::spawn(
                    file!(),
                    line!(),
                    async move {
                        let Ok(_guard) = housekeeper.slots.updating_refreshed_validators.try_lock()
                        else {
                            warn!("refreshed validators update already in progress");
                            return;
                        };
                        let start = Instant::now();
                        if let Err(err) = housekeeper.refresh_known_validators().await {
                            error!(%err, "failed to refresh known validators");
                        } else {
                            housekeeper.slots.update_refreshed_validators(head_slot);
                        }
                        info!(duration = ?start.elapsed(), "refresh validators task completed");
                    }
                    .in_current_span(),
                );
            }
        }

        // builder info updated every slot
        let housekeeper = self.clone();
        task::spawn(
            file!(),
            line!(),
            async move {
                let Ok(_guard) = housekeeper.slots.updating_builder_infos.try_lock() else {
                    warn!("builder info update already in progress");
                    return;
                };
                let start = Instant::now();
                if let Err(err) = housekeeper.sync_builder_info_changes().await {
                    error!(%err, "failed to sync builder info changes");
                }
                info!(duration = ?start.elapsed(), "sync builder info task completed");
            }
            .in_current_span(),
        );

        // trusted proposers
        if self.should_update_trusted_proposers(head_slot.as_u64()) {
            if is_local_dev() {
                warn!("skipping refresh of trusted proposers")
            } else {
                let housekeeper = self.clone();
                task::spawn(
                    file!(),
                    line!(),
                    async move {
                        let Ok(_guard) = housekeeper.slots.updating_trusted_proposers.try_lock() else {
                            warn!("trusted proposer update already in progress");
                            return;
                        };
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
        }
    }

    #[tracing::instrument(skip_all, name = "update_proposer_duties", fields(epoch = %epoch))]
    async fn update_proposer_duties(
        &self,
        head_slot: Slot,
        epoch: Epoch,
        block_hash: Option<B256>,
    ) {
        let (proposer_duties, validator_registrations) =
            match self.fetch_proposer_duties_and_registrations(epoch).await {
                Ok(result) => result,
                Err(err) => {
                    error!(%err, "failed to fetch proposer duties and registrations");
                    return;
                }
            };

        // Update primev if service exists
        if let Some(primev_s) = self.primev_service.clone() {
            let db = self.db.clone();
            let auctioneer = self.auctioneer.clone();
            let duties = proposer_duties.clone();
            // don't wait for primev update to complete
            task::spawn(
                file!(),
                line!(),
                async move {
                    if let Err(err) =
                        Self::primev_update_with_duties(primev_s, auctioneer, db, duties).await
                    {
                        error!(%err, "failed to update primev duties");
                    }
                }
                .in_current_span(),
            );
        }

        if validator_registrations.is_empty() {
            warn!(%epoch, "no validator registrationts found");
            return;
        }

        if let Err(err) =
            self.update_proposer_duties_in_db(&proposer_duties, &validator_registrations).await
        {
            error!(%err, "failed to update proposer duties");
        }

        self.slots.update_proposer_duties(head_slot);

        if let Some(inclusion_list_service) = self.inclusion_list_service.as_ref() &&
            let Some(next_duty) = proposer_duties.iter().find(|duty| duty.slot == head_slot)
        {
            let pub_key = next_duty.pubkey;
            let inclusion_list_service = inclusion_list_service.clone();
            task::spawn(
                file!(),
                line!(),
                async move {
                    inclusion_list_service
                        .handle_inclusion_list_for_slot(block_hash, pub_key, head_slot.as_u64())
                        .await
                }
                .in_current_span(),
            );
        }
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
        let mut builder_infos = self.db.get_all_builder_infos().await?;
        debug!(builder_infos = builder_infos.len(), "updating builder infos");

        if is_local_dev() {
            builder_infos.extend_from_slice(self.local_builders.as_slice());
        }

        self.auctioneer.update_builder_infos(&builder_infos, true);

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
    async fn update_proposer_duties_in_db(
        &self,
        proposer_duties: &[ProposerDuty],
        signed_validator_registrations: &HashMap<
            BlsPublicKeyBytes,
            SignedValidatorRegistrationEntry,
        >,
    ) -> Result<(), HousekeeperError> {
        debug!("updating proposer duties");

        let mut formatted_proposer_duties = Vec::with_capacity(proposer_duties.len());
        for duty in proposer_duties.iter() {
            if let Some(reg) = signed_validator_registrations.get(&duty.pubkey) {
                formatted_proposer_duties.push(BuilderGetValidatorsResponseEntry {
                    slot: duty.slot,
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

        self.auctioneer.update_proposer_duties(formatted_proposer_duties.clone());

        if is_local_dev() {
            warn!("skipping proposer duty update in db");
        } else {
            self.db.set_proposer_duties(formatted_proposer_duties).await?;
        }

        Ok(())
    }

    fn should_update_duties(&self, head_slot: Slot) -> bool {
        let last_updated = self.slots.proposer_duties();
        head_slot.as_u64().saturating_sub(last_updated) >= PROPOSER_DUTIES_UPDATE_FREQ
    }

    /// Updates primev builders and validators using pre-fetched proposer duties
    async fn primev_update_with_duties(
        primev_service: EthereumPrimevService,
        auctioneer: Arc<LocalCache>,
        db: Arc<DB>,
        proposer_duties: Vec<ProposerDuty>,
    ) -> Result<(), HousekeeperError> {
        let primev_validators =
            primev_service.get_registered_primev_validators(proposer_duties).await;
        info!(primev_validators = primev_validators.len(), "updating primev proposers");
        auctioneer.update_primev_proposers(&primev_validators);

        let primev_builders = primev_service.get_registered_primev_builders().await;

        let mut primev_builders_config: Vec<BuilderConfig> = Vec::new();

        for builder_pubkey in primev_builders {
            match auctioneer.get_builder_info(&builder_pubkey) {
                Some(builder_info) => {
                    if builder_info.builder_id == Some("PrimevBuilder".to_string()) ||
                        builder_info
                            .builder_ids
                            .as_ref()
                            .is_some_and(|v| v.contains(&"PrimevBuilder".to_string()))
                    {
                        // If the builder is already registered as PrimevBuilder, we skip it.
                        continue;
                    }
                    let builder_config = BuilderConfig {
                        pub_key: builder_pubkey,
                        builder_info: BuilderInfo {
                            collateral: builder_info.collateral,
                            is_optimistic: builder_info.is_optimistic,
                            is_optimistic_for_regional_filtering: builder_info
                                .is_optimistic_for_regional_filtering,
                            builder_id: builder_info.builder_id.clone(),
                            builder_ids: {
                                match builder_info.builder_ids {
                                    Some(ids) => {
                                        let mut ids = ids;
                                        if !ids.contains(&"PrimevBuilder".to_string()) {
                                            ids.push("PrimevBuilder".to_string());
                                        }
                                        Some(ids)
                                    }
                                    None => Some(vec!["PrimevBuilder".to_string()]),
                                }
                            },
                            api_key: None,
                        },
                    };
                    primev_builders_config.push(builder_config);
                }
                None => {
                    let builder_config = BuilderConfig {
                        pub_key: builder_pubkey,
                        builder_info: BuilderInfo {
                            collateral: U256::ZERO,
                            is_optimistic: false,
                            is_optimistic_for_regional_filtering: false,
                            builder_id: Some("PrimevBuilder".to_string()),
                            builder_ids: Some(vec!["PrimevBuilder".to_string()]),
                            api_key: None,
                        },
                    };
                    primev_builders_config.push(builder_config)
                }
            }
        }

        auctioneer.update_builder_infos(&primev_builders_config, false);

        db.store_builders_info(primev_builders_config.as_slice()).await?;

        Ok(())
    }

    fn should_update_trusted_proposers(&self, head_slot: u64) -> bool {
        let last_updated = self.slots.trusted_proposers();
        head_slot.is_multiple_of(TRUSTED_PROPOSERS_UPDATE_FREQ) ||
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
        self.auctioneer.update_trusted_proposers(proposer_whitelist);

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
        pubkeys: &[&BlsPublicKeyBytes],
    ) -> Result<HashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>, HousekeeperError>
    {
        let registrations: Vec<SignedValidatorRegistrationEntry> =
            self.db.get_validator_registrations_for_pub_keys(pubkeys).await?;

        let registrations =
            registrations.into_iter().map(|entry| (*entry.public_key(), entry)).collect();

        Ok(registrations)
    }

    /// Fetches proposer duties and validator registrations for the given epoch
    async fn fetch_proposer_duties_and_registrations(
        &self,
        epoch: Epoch,
    ) -> Result<
        (Vec<ProposerDuty>, HashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>),
        HousekeeperError,
    > {
        let proposer_duties = self.fetch_duties(epoch.as_u64()).await?;
        let pubkeys: Vec<&BlsPublicKeyBytes> =
            proposer_duties.iter().map(|duty| &duty.pubkey).collect();
        let signed_validator_registrations =
            self.fetch_signed_validator_registrations(&pubkeys).await?;

        Ok((proposer_duties, signed_validator_registrations))
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
