use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use ethereum_consensus::primitives::BlsPublicKey;
use ethers::{
    abi::{Abi, AbiParser, Address, Bytes},
    contract::{Contract, EthEvent},
    providers::{Http, Provider},
    types::U256,
};
use reth_primitives::{constants::EPOCH_SLOTS, revm_primitives::HashSet};
use std::convert::TryFrom;
use tokio::{
    sync::{broadcast, Mutex},
    time::{interval_at, sleep, Instant},
};
use tracing::{debug, error, info, warn};

use helix_beacon_client::{
    error::BeaconClientError,
    types::{HeadEventData, StateId},
    MultiBeaconClientTrait,
};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo,
    pending_block::PendingBlock, BuilderInfo, PrimevConfig, ProposerDuty, RelayConfig, Route,
    SignedValidatorRegistrationEntry,
};
use helix_database::{error::DatabaseError, DatabaseService};
use helix_datastore::Auctioneer;

use crate::error::HousekeeperError;
use uuid::Uuid;

const PROPOSER_DUTIES_UPDATE_FREQ: u64 = 1;

const TRUSTED_PROPOSERS_UPDATE_FREQ: u64 = 5;

const CUTT_OFF_TIME: u64 = 4;

// Constants for known validators refresh logic.
const MIN_SLOTS_BETWEEN_UPDATES: u64 = 6;
const MAX_SLOTS_BEFORE_FORCED_UPDATE: u64 = 32;
pub(crate) const SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS: Duration = Duration::from_secs(6);

// Max time between header and payload for OptimsiticV2 submissions
const MAX_DELAY_BETWEEN_V2_SUBMISSIONS_MS: u64 = 2_000;

// Max time to wait for payload after header is received for OptimsiticV2 submissions
const MAX_DELAY_WITH_NO_V2_PAYLOAD_MS: u64 = 20_000;

/// Arc wrapped Housekeeper type for convenience
type SharedHousekeeper<Database, BeaconClient, Auctioneer> =
    Arc<Housekeeper<Database, BeaconClient, Auctioneer>>;

/// Housekeeper Service.
///
/// Responsible for updating and managing known validators and proposer duties.
/// Also responsible for keeping the Auctioneer builder info up to date after manual changes.
/// If running multiple API instances in a single region only one housekeeper is needed as services
/// will sync through db.
pub struct Housekeeper<
    DB: DatabaseService + 'static,
    BeaconClient: MultiBeaconClientTrait + 'static,
    A: Auctioneer + 'static,
> {
    db: Arc<DB>,
    beacon_client: BeaconClient,
    auctioneer: A,

    head_slot: Mutex<u64>,

    proposer_duties_slot: Mutex<u64>,
    proposer_duties_lock: Mutex<()>,

    refreshed_validators_slot: Mutex<u64>,
    refresh_validators_lock: Mutex<()>,

    re_sync_builder_info_slot: Mutex<u64>,
    re_sync_builder_info_lock: Mutex<()>,

    refreshed_trusted_proposers_slot: Mutex<u64>,
    refresh_trusted_proposers_lock: Mutex<()>,

    leader_id: String,

    config: RelayConfig,

    chain_info: Arc<ChainInfo>,
}

impl<DB: DatabaseService, BeaconClient: MultiBeaconClientTrait, A: Auctioneer>
    Housekeeper<DB, BeaconClient, A>
{
    pub fn new(
        db: Arc<DB>,
        beacon_client: BeaconClient,
        auctioneer: A,
        config: RelayConfig,
        chain_info: Arc<ChainInfo>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            beacon_client,
            auctioneer,
            head_slot: Mutex::new(0),
            proposer_duties_slot: Mutex::new(0),
            proposer_duties_lock: Mutex::new(()),
            refreshed_validators_slot: Mutex::new(0),
            refresh_validators_lock: Mutex::new(()),
            re_sync_builder_info_slot: Mutex::new(0),
            re_sync_builder_info_lock: Mutex::new(()),
            refreshed_trusted_proposers_slot: Mutex::new(0),
            refresh_trusted_proposers_lock: Mutex::new(()),
            leader_id: Uuid::new_v4().to_string(),
            config,
            chain_info,
        })
    }

    /// Start the Housekeeper service.
    pub async fn start(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_event_receiver: &mut broadcast::Receiver<HeadEventData>,
    ) -> Result<(), BeaconClientError> {
        let best_sync_status = self.beacon_client.best_sync_status().await?;

        self.process_new_slot(best_sync_status.head_slot).await;
        loop {
            let start_instant = Instant::now() +
                self.chain_info.clock.duration_until_next_slot() +
                Duration::from_secs(CUTT_OFF_TIME);
            let mut timer =
                interval_at(start_instant, Duration::from_secs(self.chain_info.seconds_per_slot));

            tokio::select! {
                head_event_result = head_event_receiver.recv() => {
                    match head_event_result {
                        Ok(head_event) => {
                            self.process_new_slot(head_event.slot).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("head events lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            error!("head event channel closed");
                            break;
                        }
                    }
                }
                _ = timer.tick() => {
                    match self.chain_info.clock.current_slot() {
                        Some(slot) => self.process_new_slot(slot).await,
                        None => {
                            error!("could not get current slot");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Process updates for the given slot.
    ///
    /// Skips slots that are older than the currently processed slot.
    async fn process_new_slot(self: &SharedHousekeeper<DB, BeaconClient, A>, head_slot: u64) {
        let (is_new_block, prev_head_slot) = self.update_head_slot(head_slot).await;
        if !is_new_block {
            return
        }

        // Skip processing if the GetPayload route is enabled.
        if self.config.router_config.enabled_routes.iter().any(|r| r.route == Route::GetPayload) {
            return
        }

        // Only allow one housekeeper task to run at a time.
        if !self.auctioneer.try_acquire_or_renew_leadership(&self.leader_id).await {
            return
        }

        // Demote builders with expired pending blocks
        let cloned_self = self.clone();
        tokio::spawn(async move {
            if let Err(err) = cloned_self.demote_builders_with_expired_pending_blocks().await {
                error!(err = %err, "failed to demote builders with expired pending blocks");
            }
        });

        // Spawn a task to asynchronously update proposer duties.
        if self.should_update_duties(head_slot).await {
            let cloned_self = self.clone();
            tokio::spawn(async move {
                let _ = cloned_self.update_proposer_duties(head_slot).await;
            });
        }

        // Spawn a task to asynchronously update known validators.
        if self.should_refresh_known_validators(head_slot).await {
            let cloned_self = self.clone();
            tokio::spawn(async move {
                let _ = cloned_self.refresh_known_validators(head_slot).await;
            });
        }

        // Spawn a task to asynchronously re sync builder info.
        let cloned_self = self.clone();
        tokio::spawn(async move {
            let _ = cloned_self.sync_builder_info_changes(head_slot).await;
        });

        if self.config.primev_config.is_some() {
            let cloned_self = self.clone();
            tokio::spawn(async move {
                let _ = cloned_self.primev_update().await;
            });
        }

        // Spawn a task to asynchronously update the trusted proposers.
        if self.should_update_trusted_proposers(head_slot).await {
            let cloned_self = self.clone();
            tokio::spawn(async move {
                let _ = cloned_self.update_trusted_proposers(head_slot).await;
            });
        }

        debug!(
            head_slot = head_slot,
            head_slot_pos = (head_slot % EPOCH_SLOTS) + 1,
            prev_head_slot = prev_head_slot,
            "Housekeeper::process_new_slot",
        );

        // Log any missed slots and all slots that have been updated.
        if prev_head_slot > 0 {
            for missed_slot in prev_head_slot + 1..head_slot {
                warn!(missed_slot = missed_slot);
            }
        }

        let current_epoch = head_slot / EPOCH_SLOTS;
        debug!(
            epoch = current_epoch,
            slot_start_next_epoch = (current_epoch + 1) * EPOCH_SLOTS,
            head_slot = head_slot,
            "updated head slot",
        );
    }

    /// Update the head slot and return whether the given slot is a new block.
    ///
    /// - Acquires a lock on `head_slot`.
    /// - Compares the given `head_slot` with the current value.
    /// - Updates the value if the given `head_slot` is greater.
    ///
    /// Returns a tuple containing:
    /// - A boolean that indicates whether the given slot is a new block (`true`) or not (`false`).
    /// - The value of the previous head slot.
    async fn update_head_slot(&self, head_slot: u64) -> (bool, u64) {
        let mut guard = self.head_slot.lock().await;
        let prev_head_slot = *guard;
        if prev_head_slot < head_slot {
            *guard = head_slot;
            (true, prev_head_slot)
        } else {
            (false, prev_head_slot)
        }
    }

    /// Refresh the list of known validators by querying the beacon client.
    /// Refreshed validators are stored in the database.
    ///
    /// This will lock `known_validators_lock` to ensure that only one task is refreshing the known
    /// validators at a time.
    async fn refresh_known_validators(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> Result<(), HousekeeperError> {
        let _guard = self.refresh_validators_lock.try_lock()?;

        // Wait for 6s into the slot
        sleep(SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS).await;

        debug!(
            head_slot = head_slot,
            head_slot_pos = (head_slot % EPOCH_SLOTS) + 1,
            "Housekeeper::refresh_known_validators",
        );

        let start_fetching_ts = Instant::now();

        let validators = match self.beacon_client.get_state_validators(StateId::Head).await {
            Ok(validators) => validators,
            Err(err) => {
                error!(err = %err, "failed to fetch validators");
                return Err(HousekeeperError::BeaconClientError(err))
            }
        };

        info!(
            head_slot = head_slot,
            num_known_validators = validators.len(),
            fetch_validators_latency_ms = start_fetching_ts.elapsed().as_millis(),
        );

        if let Err(err) = self.db.set_known_validators(validators).await {
            error!(err = %err, "failed to set known validators");
            return Err(HousekeeperError::DatabaseError(err))
        }

        *self.refreshed_validators_slot.lock().await = head_slot;

        Ok(())
    }

    /// Synchronizes builder information changes.
    async fn sync_builder_info_changes(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> Result<(), HousekeeperError> {
        let _guard = self.re_sync_builder_info_lock.try_lock()?;

        debug!(
            head_slot = head_slot,
            head_slot_pos = (head_slot % EPOCH_SLOTS) + 1,
            "Housekeeper::sync_builder_info_changes",
        );

        let start_fetching_ts = Instant::now();

        let builder_infos = match self.db.get_all_builder_infos().await {
            Ok(builder_infos) => builder_infos,
            Err(err) => {
                error!(err = %err, "failed to fetch builder infos");
                return Err(HousekeeperError::DatabaseError(err))
            }
        };

        if let Err(err) = self.auctioneer.update_builder_infos(builder_infos).await {
            error!(err = %err, "failed to update builder infos in auctioneer");
            return Err(HousekeeperError::AuctioneerError(err))
        }

        *self.re_sync_builder_info_slot.lock().await = head_slot;

        info!(head_slot = head_slot, update_latency_ms = start_fetching_ts.elapsed().as_millis());
        Ok(())
    }

    /// Handle valid payload Optimistic V2 demotions.
    ///
    /// There are two cases where we might demote a builder here.
    /// 1) They sent a header but we received no accompanying payload.
    /// 2) The payload was received > 2 seconds after we received the header.
    ///
    /// DB entries are also removed if they have been waiting for over 45 seconds.
    async fn demote_builders_with_expired_pending_blocks(&self) -> Result<(), HousekeeperError> {
        let current_time =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64;

        let mut demoted_builders = HashSet::new();

        let mut pending_block_hashes = HashMap::new();

        for pending_block in self.auctioneer.get_pending_blocks().await? {
            pending_block_hashes
                .entry(pending_block.builder_pubkey.clone())
                .or_insert_with(Vec::new)
                .push(pending_block.block_hash.clone());

            if demoted_builders.contains(&pending_block.builder_pubkey) {
                continue
            }

            if v2_submission_late(&pending_block, current_time) {
                let reason =
                    format!("builder demoted due to missing payload submission. {pending_block:?}");
                info!(builder_pub_key = ?pending_block.builder_pubkey, reason);
                self.auctioneer.demote_builder(&pending_block.builder_pubkey).await?;
                self.db
                    .db_demote_builder(
                        &pending_block.builder_pubkey,
                        &pending_block.block_hash,
                        reason.to_string(),
                    )
                    .await?;
                demoted_builders.insert(pending_block.builder_pubkey);
            }
        }

        Ok(())
    }

    /// Determine if known validators should be refreshed for the given slot.
    async fn should_refresh_known_validators(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> bool {
        let last_refreshed_slot = *self.refreshed_validators_slot.lock().await;

        if head_slot <= last_refreshed_slot {
            return false
        }

        let slots_since_last_update = head_slot - last_refreshed_slot;
        if slots_since_last_update < MIN_SLOTS_BETWEEN_UPDATES {
            return false
        }

        let force_update = slots_since_last_update > MAX_SLOTS_BEFORE_FORCED_UPDATE;
        if force_update {
            return true
        }

        let head_slot_pos = (head_slot % EPOCH_SLOTS) + 1; // position in epoch.
        head_slot_pos == 4 || head_slot_pos == 20
    }

    /// Update proposer duties for `head_slot` and `head_slot` + 1.
    async fn update_proposer_duties(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> Result<(), HousekeeperError> {
        // Only allow one update_proposer_duties task at a time.
        let _guard = self.proposer_duties_lock.try_lock()?;

        let epoch = head_slot / EPOCH_SLOTS;

        info!(epoch_from = epoch, epoch_to = epoch + 1, "Housekeeper::update_proposer_duties",);

        let proposer_duties = match self.fetch_duties(epoch).await {
            Ok(proposer_duties) => proposer_duties,
            Err(err) => {
                error!(err = %err, "failed to fetch proposer duties");
                return Err(HousekeeperError::BeaconClientError(err))
            }
        };

        // Check if signed validator registrations exist for each proposer duty
        let pub_keys: Vec<BlsPublicKey> =
            proposer_duties.iter().map(|duty| duty.public_key.clone()).collect();
        let signed_validator_registrations =
            match self.fetch_signed_validator_registrations(pub_keys).await {
                Ok(signed_validator_registrations) => signed_validator_registrations,
                Err(err) => {
                    error!(err = %err, "failed to fetch signed validator registrations");
                    return Err(HousekeeperError::DatabaseError(err))
                }
            };

        // Format duties and save to the database
        if signed_validator_registrations.is_empty() {
            warn!("No signed validator registrations found for proposer duties");
        } else {
            match self
                .format_and_store_duties(proposer_duties, signed_validator_registrations)
                .await
            {
                Ok(num_duties) => {
                    info!(epoch_from = epoch, num_duties = num_duties, "updated proposer duties")
                }
                Err(err) => error!(err = %err, "failed to update proposer duties"),
            }
        }

        *self.proposer_duties_slot.lock().await = head_slot;

        Ok(())
    }

    /// Format and store proposer duties
    ///
    /// Returns the number of proposer duties registered to the relay for the next 2 epochs.
    pub async fn format_and_store_duties(
        &self,
        proposer_duties: Vec<ProposerDuty>,
        signed_validator_registrations: HashMap<BlsPublicKey, SignedValidatorRegistrationEntry>,
    ) -> Result<usize, DatabaseError> {
        let mut formatted_proposer_duties: Vec<BuilderGetValidatorsResponseEntry> =
            Vec::with_capacity(proposer_duties.len());

        for duty in proposer_duties {
            if let Some(reg) = signed_validator_registrations.get(&duty.public_key) {
                if duty.public_key != reg.registration_info.registration.message.public_key {
                    error!(?duty, ?reg, "mismatch in duty vs registration")
                }

                formatted_proposer_duties.push(BuilderGetValidatorsResponseEntry {
                    slot: duty.slot,
                    validator_index: duty.validator_index,
                    entry: reg.registration_info.clone(),
                });
            }
        }

        let num_duties = formatted_proposer_duties.len();

        self.db.set_proposer_duties(formatted_proposer_duties).await?;

        Ok(num_duties)
    }

    /// Determine if proposer duties should be updated for the given slot.
    ///
    /// This function checks:
    /// If the distance between the current `head_slot` and the last slot for which proposer
    /// duties were fetched (`proposer_duties_slot`) is greater than or equal to
    /// PROPOSER_DUTIES_UPDATE_FREQ, it will also return `true`.
    async fn should_update_duties(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> bool {
        let proposer_duties_slot = *self.proposer_duties_slot.lock().await;
        let last_proposer_duty_distance = head_slot.saturating_sub(proposer_duties_slot);
        last_proposer_duty_distance >= PROPOSER_DUTIES_UPDATE_FREQ
    }

    async fn primev_update(&self) -> Result<(), HousekeeperError> {
        let primev_config = self.config.primev_config.as_ref().unwrap();
        let primev_builders = get_registered_primev_builders(primev_config).await;
        for builder_pubkey in primev_builders {
            self.db
                .store_builder_info(
                    &builder_pubkey,
                    &BuilderInfo {
                        collateral: ethereum_consensus::primitives::U256::from(0),
                        is_optimistic: false,
                        builder_id: Some("PrimevBuilder".to_string()),
                    },
                )
                .await?;
        }

        let primev_validators = get_registered_primev_validators(primev_config).await;
        self.auctioneer.update_primev_proposers(&primev_validators).await?;

        let primev_builder_pref = vec!["PrimevBuilder".to_string()];
        self.db.update_trusted_builders(&primev_validators, &primev_builder_pref).await?;

        Ok(())
    }

    /// Determine if the trusted proposers should be refreshed for the given slot.
    ///     
    /// This function checks two conditions:
    /// 1. If the `head_slot` is exactly divisible by `TRUSTED_PROPOSERS_UPDATE_FREQ`,
    ///   it will return `true` to trigger a trusted proposer update.
    /// 2. If the distance between the current `head_slot` and the last slot for which
    ///  the trusted proposers was refreshed (`refreshed_trusted_proposers_slot`) is greater than or
    /// equal to `TRUSTED_PROPOSERS_UPDATE_FREQ`, it will also return `true`.
    async fn should_update_trusted_proposers(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> bool {
        let trusted_proposers_slot = *self.refreshed_trusted_proposers_slot.lock().await;
        let last_trusted_proposers_distance = head_slot.saturating_sub(trusted_proposers_slot);
        head_slot % TRUSTED_PROPOSERS_UPDATE_FREQ == 0 ||
            last_trusted_proposers_distance >= TRUSTED_PROPOSERS_UPDATE_FREQ
    }

    /// Update the proposer whitelist.
    ///
    /// This function will fetch the proposer whitelist from the database and update the auctioneer.
    /// It will also update the `refreshed_trusted_proposers_slot` to the current `head_slot`.
    ///
    /// This function will error if it cannot fetch the proposer whitelist from the database.
    /// It will continue if it cannot update the auctioneer.
    ///
    /// This function will also error if it cannot update the `refreshed_trusted_proposers_slot`.
    ///
    /// This function will return `Ok(())` if it completes successfully.
    async fn update_trusted_proposers(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        head_slot: u64,
    ) -> Result<(), HousekeeperError> {
        let _guard = self.refresh_trusted_proposers_lock.try_lock()?;

        debug!(head_slot = head_slot, "Housekeeper::update_trusted_proposers",);

        let proposer_whitelist = self.db.get_trusted_proposers().await?;
        let num_trusted_proposers = proposer_whitelist.len();

        self.auctioneer.update_trusted_proposers(proposer_whitelist).await?;
        *self.refreshed_trusted_proposers_slot.lock().await = head_slot;

        debug!(
            head_slot = head_slot,
            num_trusted_proposers = num_trusted_proposers,
            "updated trusted proposers"
        );

        Ok(())
    }

    /// Fetch proposer duties for the given epoch and epoch + 1.
    ///
    /// This function will error if it cannot fetch the duties for the current epoch
    /// but will continue if it fails to fetch epoch + 1.
    async fn fetch_duties(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        epoch: u64,
    ) -> Result<Vec<ProposerDuty>, BeaconClientError> {
        // Fetch duties for current epoch
        let (_, mut proposer_duties) = self.beacon_client.get_proposer_duties(epoch).await?;

        // Fetch duties for next epoch
        match self.beacon_client.get_proposer_duties(epoch + 1).await {
            Ok((_, mut next_duties)) => proposer_duties.append(&mut next_duties),
            Err(err) => error!(err = %err, "Error fetching next proposer duties"),
        }

        Ok(proposer_duties)
    }

    /// Fetch validator registrations for `pub_keys` from database.
    async fn fetch_signed_validator_registrations(
        self: &SharedHousekeeper<DB, BeaconClient, A>,
        pub_keys: Vec<BlsPublicKey>,
    ) -> Result<HashMap<BlsPublicKey, SignedValidatorRegistrationEntry>, DatabaseError> {
        let registrations: Vec<SignedValidatorRegistrationEntry> =
            self.db.get_validator_registrations_for_pub_keys(pub_keys).await?;
        Ok(registrations.into_iter().map(|entry| (entry.public_key().clone(), entry)).collect())
    }
}

/// Calculates the delay in submission of the payload after a header.
///
/// Returns true if the payload was received over 2 seconds after the header or if the payload was
/// never received. Otherwise, returns false.
fn v2_submission_late(pending_block: &PendingBlock, current_time: u64) -> bool {
    match (pending_block.header_receive_ms, pending_block.payload_receive_ms) {
        (None, None) => false,
        (None, Some(_)) => false,
        (Some(header_receive_ms), None) => {
            (current_time.saturating_sub(header_receive_ms)) > MAX_DELAY_WITH_NO_V2_PAYLOAD_MS
        }
        (Some(header_receive_ms), Some(payload_receive_ms)) => {
            payload_receive_ms.saturating_sub(header_receive_ms) >
                MAX_DELAY_BETWEEN_V2_SUBMISSIONS_MS
        }
    }
}

#[derive(Debug, EthEvent)]
#[ethevent(
    abi = "ProviderRegistered(address indexed provider, uint256 stakedAmount, bytes blsPublicKey)"
)]
pub struct ValueChanged {
    #[ethevent(indexed, name = "provider")]
    pub provider: Address,
    #[ethevent(name = "stakedAmount")]
    pub staked_amount: U256,
    #[ethevent(name = "blsPublicKey")]
    pub bls_public_key: Vec<u8>,
}

/// Fetches the registered primev builders from the provider registry contract.
/// Returns a list of builder pubkeys.
/// Currently returns the wrong type, need to fix. Primev working on this.
pub async fn get_registered_primev_builders(config: &PrimevConfig) -> Vec<BlsPublicKey> {
    let provider = Provider::<Http>::try_from(config.builder_url.as_str()).unwrap();
    let provider = Arc::new(provider);

    // Define the contract address and ABI
    let provider_registry_address: Address = config.builder_contract.as_str().parse().unwrap();
    // let abi: Abi =
    // serde_json::from_str(r#"[{"anonymous":false,"inputs":[{"indexed":true,"internalType":"
    // address","name":"provider","type":"address"},{"indexed":false,"internalType":"uint256","name"
    // :"stakedAmount","type":"uint256"}],"name":"ProviderRegistered","type":"event"}]"#).unwrap();

    let abi_human_readable = r#"
    [
        "event ProviderRegistered(address indexed provider, uint256 stakedAmount, bytes blsPublicKey)"
    ]
    "#;

    // Parse the ABI
    let abi: Abi = AbiParser::default().parse_str(abi_human_readable).unwrap();

    // Create a new contract instance
    let contract = Contract::new(provider_registry_address, abi, provider.clone());

    let event = contract
        .event_for_name("ProviderRegistered")
        .unwrap()
        .from_block(0)
        .address(provider_registry_address.into());

    let providers: Vec<ValueChanged> = match event.query().await {
        Ok(providers) => providers,
        Err(err) => {
            println!("{:?}", err);
            Vec::new()
        }
    };

    let mut bls_public_keys = Vec::new();
    for i in providers.iter() {
        bls_public_keys.push(BlsPublicKey::try_from(i.bls_public_key.as_slice()).unwrap());
    }
    bls_public_keys
}

/// Fetches the registered primev validators from the validators registry contract.
pub async fn get_registered_primev_validators(config: &PrimevConfig) -> Vec<BlsPublicKey> {
    let provider = Provider::<Http>::try_from(config.validator_url.as_str()).unwrap();
    let provider = Arc::new(provider);

    // Define the contract address and ABI
    let provider_registry_address: Address = config.validator_contract.as_str().parse().unwrap();

    let abi_str = r#"
    [
        {
            "type": "function",
            "name": "getStakedValidators",
            "inputs": [
                {
                    "name": "start",
                    "type": "uint256",
                    "internalType": "uint256"
                },
                {
                    "name": "end",
                    "type": "uint256",
                    "internalType": "uint256"
                }
            ],
            "outputs": [
                {
                    "name": "",
                    "type": "bytes[]",
                    "internalType": "bytes[]"
                },
                {
                    "name": "",
                    "type": "uint256",
                    "internalType": "uint256"
                }
            ],
            "stateMutability": "view"
        },
        {
            "type": "function",
            "name": "getNumberOfStakedValidators",
            "inputs": [],
            "outputs": [
                {
                    "name": "",
                    "type": "uint256",
                    "internalType": "uint256"
                },
                {
                    "name": "",
                    "type": "uint256",
                    "internalType": "uint256"
                }
            ],
            "stateMutability": "view"
        }
    ]
    "#;

    let abi: Abi = serde_json::from_str(abi_str).unwrap();

    // Create a new contract instance
    let contract = Contract::new(provider_registry_address, abi, provider.clone());

    let num_validators =
        match contract.method::<(), (U256, U256)>("getNumberOfStakedValidators", ()) {
            Ok(method) => match method.call().await {
                Ok((num_validators, _version)) => num_validators,
                Err(e) => {
                    error!("Error calling method: {:?}", e);
                    U256::from(0)
                }
            },
            Err(e) => {
                error!("Error calling method: {:?}", e);
                U256::from(0)
            }
        };

    let batch_size: U256 = U256::from(500);
    let mut start: U256 = U256::from(0);
    let mut total_validators: Vec<Bytes> = Vec::new();

    while start < num_validators {
        let end =
            if start + batch_size > num_validators { num_validators } else { start + batch_size };

        match contract
            .method::<(U256, U256), (Vec<Bytes>, U256)>("getStakedValidators", (start, end))
        {
            Ok(method) => match method.call().await {
                Ok(result) => {
                    if result.0.is_empty() {
                        break // No more validators to fetch
                    }
                    total_validators.extend(result.0);
                }
                Err(e) => {
                    eprintln!("Error calling method: {:?}", e);
                    break
                }
            },
            Err(e) => {
                eprintln!("Error creating method: {:?}", e);
                break
            }
        }

        start = end;
    }

    total_validators.iter().map(|bytes| BlsPublicKey::try_from(bytes.as_slice()).unwrap()).collect()
}
