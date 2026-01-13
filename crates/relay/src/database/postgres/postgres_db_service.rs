use std::{
    ops::DerefMut,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use alloy_primitives::{B256, U256};
use deadpool_postgres::{Config, GenericClient, ManagerConfig, Pool, RecyclingMethod};
use helix_common::{
    DataAdjustmentsEntry, Filtering, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace,
    PostgresConfig, ProposerInfo, RelayConfig, SignedValidatorRegistrationEntry, SubmissionTrace,
    ValidatorPreferences, ValidatorSummary,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        data_api::{
            BidFilters, DataAdjustmentsResponse, MergedBlockResponse,
            ProposerHeaderDeliveredParams, ProposerHeaderDeliveredResponse,
        },
        proposer_api::GetHeaderParams,
    },
    bid_submission::OptimisticVersion,
    local_cache::LocalCache,
    metrics::DbMetricRecord,
    utils::{alert_discord, utcnow_ms},
};
use helix_types::{
    BlsPublicKeyBytes, MergedBlock, SignedBidSubmission, Slot,
};
use rustc_hash::FxHashSet;
use tokio::sync::oneshot;
use tokio_postgres::{NoTls, types::ToSql};
use tracing::{error, info, instrument, warn};

use crate::database::{
    error::DatabaseError,
    postgres::{
        postgres_db_filters::PgBidFilters,
        postgres_db_init::run_migrations_async,
        postgres_db_row_parsing::{parse_bytes_to_pubkey_bytes, parse_row, parse_rows},
        postgres_db_u256_parsing::PostgresNumeric,
    },
    types::{
        BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument, SavePayloadParams,
    },
};

pub enum DbRequest {
    GetValidatorRegistration {
        pub_key: BlsPublicKeyBytes,
        response: oneshot::Sender<Result<SignedValidatorRegistrationEntry, DatabaseError>>,
    },
    GetValidatorRegistrationsForPubKeys {
        pub_keys: Vec<BlsPublicKeyBytes>,
        response: oneshot::Sender<Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>>,
    },
    SaveTooLateGetPayload {
        slot: u64,
        proposer_pub_key: BlsPublicKeyBytes,
        payload_hash: B256,
        message_received: u64,
        payload_fetched: u64,
    },
    SaveDeliveredPayload {
        params: SavePayloadParams,
    },
    StoreBuildersInfo {
        builders: Vec<BuilderInfoDocument>,
    },
    DbDemoteBuilder {
        slot: u64,
        builder_pub_key: BlsPublicKeyBytes,
        block_hash: B256,
        reason: String,
        failsafe_triggered: Arc<AtomicBool>,
    },
    GetBids {
        filters: BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
        response: oneshot::Sender<Result<Vec<BidSubmissionDocument>, DatabaseError>>,
    },
    GetDeliveredPayloads {
        filters: BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
        response: oneshot::Sender<Result<Vec<DeliveredPayloadDocument>, DatabaseError>>,
    },
    SaveGetHeaderCall {
        params: GetHeaderParams,
        best_block_hash: B256,
        value: U256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    },
    SaveFailedGetPayload {
        slot: u64,
        block_hash: B256,
        error: String,
        trace: GetPayloadTrace,
    },
    SaveGossipedPayloadTrace {
        block_hash: B256,
        trace: GossipedPayloadTrace,
    },
    GetTrustedProposers {
        response: oneshot::Sender<Result<Vec<ProposerInfo>, DatabaseError>>,
    },
    SaveInclusionList {
        inclusion_list: InclusionListWithMetadata,
        slot: u64,
        block_parent_hash: B256,
        proposer_pubkey: BlsPublicKeyBytes,
    },
    GetBlockAdjustmentsForSlot {
        slot: Slot,
        response: oneshot::Sender<Result<Vec<DataAdjustmentsResponse>, DatabaseError>>,
    },
    SaveBlockAdjustmentsData {
        entry: DataAdjustmentsEntry,
    },
    GetProposerHeaderDelivered {
        params: ProposerHeaderDeliveredParams,
        response: oneshot::Sender<Result<Vec<ProposerHeaderDeliveredResponse>, DatabaseError>>,
    },
    SetKnownValidators {
        known_validators: Vec<ValidatorSummary>,
    },
    SetProposerDuties {
        duties: Vec<BuilderGetValidatorsResponseEntry>,
    },
    DisableAdjustments {
        block_hash: B256,
        failsafe_trigger: Arc<AtomicBool>,
        adjustments_enabled: Arc<AtomicBool>,
    },
}

pub struct PendingBlockSubmissionValue {
    pub submission: SignedBidSubmission,
    pub trace: SubmissionTrace,
    pub optimistic_version: OptimisticVersion,
    pub is_adjusted: bool,
}

const BLOCK_SUBMISSION_FIELD_COUNT: usize = 17;
const MAINNET_VALIDATOR_COUNT: usize = 1_100_000;
const DB_CHECK_INTERVAL: Duration = Duration::from_secs(1);
static DELIVERED_PAYLOADS_MIG_SLOT: AtomicU64 = AtomicU64::new(0);

fn new_validator_set() -> FxHashSet<BlsPublicKeyBytes> {
    FxHashSet::with_capacity_and_hasher(MAINNET_VALIDATOR_COUNT, Default::default())
}

struct RegistrationParams<'a> {
    fee_recipient: &'a [u8],
    gas_limit: i32,
    timestamp: i64,
    public_key: Vec<u8>,
    signature: Vec<u8>,
    inserted_at: SystemTime,
    user_agent: Option<String>,
}

struct PreferenceParams {
    public_key: Vec<u8>,
    filtering: i16,
    trusted_builders: Option<Vec<String>>,
    header_delay: bool,
    disable_inclusion_lists: bool,
}

struct TrustedProposerParams {
    public_key: Vec<u8>,
    name: Option<String>,
}

#[derive(Clone)]
pub struct PostgresDatabaseService {
    region: i16,
    pub pool: Arc<Pool>,
    pub high_priority_pool: Arc<Pool>,
    pub local_cache: Arc<LocalCache>,
}

impl PostgresDatabaseService {
    pub async fn from_relay_config(
        relay_config: &RelayConfig,
        local_cache: Arc<LocalCache>,
    ) -> Self {
        let mut cfg = Config::new();
        cfg.host = Some(relay_config.postgres.hostname.clone());
        cfg.port = Some(relay_config.postgres.port);
        cfg.dbname = Some(relay_config.postgres.db_name.clone());
        cfg.user = Some(relay_config.postgres.user.clone());
        cfg.password = Some(relay_config.postgres.password.clone());
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });

        let pool = loop {
            match cfg.create_pool(None, NoTls) {
                Ok(pool) => break pool,
                Err(e) => {
                    error!("Error creating pool: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        };

        let high_priority_pool = loop {
            match cfg.create_pool(None, NoTls) {
                Ok(pool) => break pool,
                Err(e) => {
                    error!("Error creating pool: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        };

        PostgresDatabaseService {
            region: relay_config.postgres.region,
            pool: Arc::new(pool),
            high_priority_pool: Arc::new(high_priority_pool),
            local_cache,
        }
    }

    /// Try to run database migrations until they succeed
    pub async fn init_forever(&self) {
        loop {
            match self.run_migrations().await {
                Ok(_) => return,
                Err(err) => {
                    error!(%err, "failed to run migrations! retrying..");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    pub async fn run_migrations(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut conn = self.pool.get().await?;

        let client = conn.deref_mut().deref_mut();
        match run_migrations_async(client).await {
            Ok(report) => {
                info!("Applied migrations: {}", report.applied_migrations().len());
                info!("Migrations report: {:?}", report);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn init_region(&self, config: &PostgresConfig) {
        let client = self.pool.get().await.unwrap();
        match client
            .execute(
                "
                INSERT INTO region (id, name)
                VALUES ($1, $2)
                ON CONFLICT (id)
                DO NOTHING
            ",
                &[&(config.region), &(config.region_name)],
            )
            .await
        {
            Ok(_) => {
                info!("Region {} initialized", config.region);
            }
            Err(e) => {
                panic!("Error initializing region {}: {}", config.region, e);
            }
        };
    }

    #[instrument(skip_all)]
    pub async fn load_known_validators(&self) {
        let mut record = DbMetricRecord::new("load_known_validators");

        let client = match self.pool.get().await {
            Ok(client) => client,
            Err(e) => {
                error!("Error getting client from pool: {}", e);
                return;
            }
        };

        let rows = match client.query("SELECT * FROM known_validators", &[]).await {
            Ok(rows) => rows,
            Err(e) => {
                error!("Error querying known_validators: {}", e);
                return;
            }
        };

        let mut set = new_validator_set();
        for row in rows {
            let public_key: BlsPublicKeyBytes =
                match parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key")) {
                    Ok(pk) => pk,
                    Err(e) => {
                        error!("Error parsing public key: {}", e);
                        continue;
                    }
                };
            set.insert(public_key);
        }

        *self.local_cache.known_validators_cache.write() = set;

        record.record_success();
    }

    #[instrument(skip_all)]
    pub async fn load_validator_registrations(&self) {
        let mut record = DbMetricRecord::new("load_validator_registrations");

        match self.get_validator_registrations().await {
            Ok(entries) => {
                let num_entries = entries.len();
                entries.into_iter().for_each(|entry| {
                    self.local_cache
                        .validator_registration_cache
                        .insert(entry.registration_info.registration.message.pubkey, entry);
                });
                info!("Loaded {} validator registrations", num_entries);
                record.record_success();
            }
            Err(e) => {
                error!("Error loading validator registrations: {}", e);
            }
        }
    }

    pub async fn start_processors(
        &mut self,
        db_request_receiver: crossbeam_channel::Receiver<DbRequest>,
        block_submission_receiver: crossbeam_channel::Receiver<PendingBlockSubmissionValue>,
    ) {
        self.start_registration_processor().await;
        self.start_block_submission_processor(block_submission_receiver).await;
        self.start_db_request_processor(db_request_receiver).await;
        self.start_check_flag_task();
    }

    fn start_check_flag_task(&mut self) {
        let svc_clone = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(DB_CHECK_INTERVAL);
            loop {
                interval.tick().await;

                if svc_clone.local_cache.adjustments_failsafe_trigger.load(Ordering::Relaxed) {
                    svc_clone.local_cache.adjustments_enabled.store(false, Ordering::Relaxed);
                    return;
                }

                match svc_clone.check_adjustments_enabled().await {
                    Ok(value) => {
                        let previous = svc_clone
                            .local_cache
                            .adjustments_enabled
                            .swap(value, Ordering::Relaxed);
                        if previous != value {
                            tracing::info!(
                                "adjustments enabled flag changed from {} to {}",
                                previous,
                                value
                            );
                        }
                    }
                    Err(e) => tracing::error!("failed to check adjustments_enabled flag: {}", e),
                }
            }
        });
    }

    pub async fn start_db_request_processor(
        &mut self,
        receiver: crossbeam_channel::Receiver<DbRequest>,
    ) {
        let svc_clone = self.clone();

        tokio::spawn(async move {
            loop {
                match receiver.recv() {
                    Ok(request) => {
                        let svc = svc_clone.clone();
                        tokio::spawn(async move {
                            svc.handle_db_request(request).await;
                        });
                    }
                    Err(e) => {
                        error!("DB request receiver error: {}", e);
                        break;
                    }
                }
            }
        });
    }

    pub async fn start_registration_processor(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                interval.tick().await;
                match self_clone.local_cache.pending_validator_registrations.len() {
                    0 => continue,
                    _ => {
                        let mut entries = Vec::new();
                        for key in self_clone.local_cache.pending_validator_registrations.iter() {
                            if let Some(entry) =
                                self_clone.local_cache.validator_registration_cache.get(&*key)
                            {
                                entries.push(entry.clone());
                            }
                        }
                        match self_clone._save_validator_registrations(&entries).await {
                            Ok(_) => {
                                for entry in entries.iter() {
                                    self_clone.local_cache.pending_validator_registrations.remove(
                                        &entry.registration_info.registration.message.pubkey,
                                    );
                                }
                                info!("Saved {} validator registrations", entries.len());
                            }
                            Err(e) => {
                                error!("Error saving validator registrations: {}", e);
                            }
                        };
                    }
                };
            }
        });
    }

    pub async fn start_block_submission_processor(
        &mut self,
        batch_receiver: crossbeam_channel::Receiver<PendingBlockSubmissionValue>,
    ) {
        let svc_clone = self.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(2_000);
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            let mut last_slot_processed = 0;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Drain all available messages from the crossbeam channel
                        while let Ok(item) = batch_receiver.try_recv() {
                            batch.push(item);
                        }

                        if !batch.is_empty() {

                            let mut retry_count = 0;
                            const MAX_RETRIES: usize = 3;

                            loop {
                                match svc_clone._flush_block_submissions(&batch, &mut last_slot_processed).await {
                                    Ok(_) => break,
                                    Err(e) => {
                                        retry_count += 1;

                                        if retry_count >= MAX_RETRIES {
                                            error!("block batch failed after {} retries: {:?}", retry_count, e);
                                            break;
                                        }

                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                }
                            }
                            batch.clear();
                        }
                    }
                }
            }
        });
    }

    pub async fn load_builder_infos(&self, local_cache: Arc<LocalCache>) {
        let mut record = DbMetricRecord::new("load_builder_infos");

        match self.get_all_builder_infos().await {
            Ok(builder_infos) => {
                local_cache.update_builder_infos(&builder_infos, true);
                info!("Loaded {} builder infos", builder_infos.len());
                record.record_success();
            }
            Err(e) => {
                error!("Error loading builder infos: {}", e);
            }
        }
    }

    pub async fn load_trusted_proposers(&self, local_cache: Arc<LocalCache>) {
        let mut record = DbMetricRecord::new("load_trusted_proposers");

        match self.get_trusted_proposers().await {
            Ok(proposers) => {
                let num_proposers = proposers.len();
                local_cache.update_trusted_proposers(proposers);
                info!("Loaded {} trusted proposers", num_proposers);
                record.record_success();
            }
            Err(e) => {
                error!("Error loading trusted proposers: {}", e);
            }
        }
    }

    async fn handle_db_request(&self, request: DbRequest) {
        match request {
            DbRequest::GetValidatorRegistration { pub_key, response } => {
                let result = self.get_validator_registration(&pub_key).await;
                let _ = response.send(result);
            }
            DbRequest::GetValidatorRegistrationsForPubKeys { pub_keys, response } => {
                let pub_key_refs: Vec<&BlsPublicKeyBytes> = pub_keys.iter().collect();
                let result = self.get_validator_registrations_for_pub_keys(&pub_key_refs).await;
                let _ = response.send(result);
            }
            DbRequest::SaveTooLateGetPayload {
                slot,
                proposer_pub_key,
                payload_hash,
                message_received,
                payload_fetched,
            } => {
                if let Err(err) = self
                    .save_too_late_get_payload(
                        slot,
                        &proposer_pub_key,
                        &payload_hash,
                        message_received,
                        payload_fetched,
                    )
                    .await
                {
                    error!(%err, "failed to save too late get payload");
                }
            }
            DbRequest::SaveDeliveredPayload { params } => {
                if let Err(err) = self.save_delivered_payload(&params).await {
                    error!(%err, "error saving payload to database");
                }
            }
            DbRequest::StoreBuildersInfo { builders } => {
                if let Err(err) = self.store_builders_info(&builders).await {
                    error!(%err, "failed to store builders info");
                }
            }
            DbRequest::DbDemoteBuilder {
                slot,
                builder_pub_key,
                block_hash,
                reason,
                failsafe_triggered,
            } => {
                if let Err(err) =
                    self.db_demote_builder(slot, &builder_pub_key, &block_hash, reason).await
                {
                    error!(%err, "error demoting builder in database triggering failsafe: stopping all optimistic submissions");
                    failsafe_triggered.store(true, Ordering::Relaxed);
                    alert_discord(&format!(
                        "{} {} {} failed to demote builder in database! Pausing all optmistic submissions",
                        builder_pub_key, err, block_hash
                    ));
                }
            }
            DbRequest::GetBids { filters, validator_preferences, response } => {
                let result = self.get_bids(&filters, validator_preferences).await;
                let _ = response.send(result);
            }
            DbRequest::GetDeliveredPayloads { filters, validator_preferences, response } => {
                let result = self.get_delivered_payloads(&filters, validator_preferences).await;
                let _ = response.send(result);
            }
            DbRequest::SaveGetHeaderCall {
                params,
                best_block_hash,
                value,
                trace,
                mev_boost,
                user_agent,
            } => {
                if let Err(err) = self
                    .save_get_header_call(
                        params,
                        best_block_hash,
                        value,
                        trace,
                        mev_boost,
                        user_agent,
                    )
                    .await
                {
                    error!(%err, "error saving get header call to database");
                }
            }
            DbRequest::SaveFailedGetPayload { slot, block_hash, error, trace } => {
                if let Err(err) = self.save_failed_get_payload(slot, block_hash, error, trace).await
                {
                    error!(err = ?err, "error saving failed get payload");
                }
            }
            DbRequest::SaveGossipedPayloadTrace { block_hash, trace } => {
                if let Err(err) = self.save_gossiped_payload_trace(block_hash, trace).await {
                    error!(%err, "failed to store gossiped payload trace")
                }
            }
            DbRequest::GetTrustedProposers { response } => {
                let result = self.get_trusted_proposers().await;
                let _ = response.send(result);
            }
            DbRequest::SaveInclusionList {
                inclusion_list,
                slot,
                block_parent_hash,
                proposer_pubkey,
            } => {
                if let Err(err) = self
                    .save_inclusion_list(
                        &inclusion_list,
                        slot,
                        &block_parent_hash,
                        &proposer_pubkey,
                    )
                    .await
                {
                    error!(%slot, "failed to save inclusion list Errors: {:?}", err);
                }
            }
            DbRequest::GetBlockAdjustmentsForSlot { slot, response } => {
                let result = self.get_block_adjustments_for_slot(slot).await;
                let _ = response.send(result);
            }
            DbRequest::SaveBlockAdjustmentsData { entry } => {
                if let Err(err) = self.save_block_adjustments_data(entry).await {
                    error!(%err, "failed to save block adjustments data");
                }
            }
            DbRequest::GetProposerHeaderDelivered { params, response } => {
                let result = self.get_proposer_header_delivered(&params).await;
                let _ = response.send(result);
            }
            DbRequest::SetKnownValidators { known_validators } => {
                if let Err(err) = self.set_known_validators(known_validators).await {
                    error!(%err, "failed to set known validators");
                }
            }
            DbRequest::SetProposerDuties { duties } => {
                if let Err(err) = self.set_proposer_duties(duties).await {
                    error!(%err, "failed to set proposer duties");
                }
            }
            DbRequest::DisableAdjustments { block_hash, failsafe_trigger, adjustments_enabled } => {
                if let Err(err) = self.disable_adjustments().await {
                    failsafe_trigger.store(true, Ordering::Relaxed);
                    adjustments_enabled.store(false, Ordering::Relaxed);
                    error!(%block_hash, %err, "failed to disable adjustments in database, pulling the failsafe trigger");
                    alert_discord(&format!(
                        "{} {} failed to disable adjustments in database, pulling the failsafe trigger",
                        err, block_hash
                    ));
                }
            }
        }
    }

    async fn _save_validator_registrations(
        &self,
        entries: &[SignedValidatorRegistrationEntry],
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_validator_registrations");

        let mut client = self.pool.get().await?;

        let mut sorted_entries = entries.to_vec();
        sorted_entries.sort_by(|a, b| {
            a.registration_info
                .registration
                .message
                .pubkey
                .cmp(&b.registration_info.registration.message.pubkey)
        });

        let batch_size = 10;
        for chunk in sorted_entries.chunks(batch_size) {
            let transaction = client.transaction().await?;

            let mut structured_params_for_reg: Vec<RegistrationParams> =
                Vec::with_capacity(chunk.len());

            let mut structured_params_for_pref: Vec<PreferenceParams> =
                Vec::with_capacity(chunk.len());

            let mut structured_params_for_trusted: Vec<TrustedProposerParams> =
                Vec::with_capacity(chunk.len());

            for entry in chunk.iter() {
                let registration = &entry.registration_info.registration.message;
                let fee_recipient = &registration.fee_recipient;
                let public_key = &registration.pubkey.to_vec();
                let signature = &entry.registration_info.registration.signature.to_vec();
                let name = &entry.pool_name;

                let inserted_at = SystemTime::now();

                // Collect the parameters in a structured manner
                structured_params_for_reg.push(RegistrationParams {
                    fee_recipient: fee_recipient.as_ref(),
                    gas_limit: registration.gas_limit as i32,
                    timestamp: registration.timestamp as i64,
                    public_key: public_key.clone(),
                    signature: signature.clone(),
                    inserted_at,
                    user_agent: entry.user_agent.clone(),
                });

                structured_params_for_pref.push(PreferenceParams {
                    public_key: public_key.clone().to_vec(),
                    filtering: entry.registration_info.preferences.filtering as i16,
                    trusted_builders: entry.registration_info.preferences.trusted_builders.clone(),
                    header_delay: entry.registration_info.preferences.header_delay,
                    disable_inclusion_lists: entry
                        .registration_info
                        .preferences
                        .disable_inclusion_lists,
                });

                if name.is_some() {
                    structured_params_for_trusted.push(TrustedProposerParams {
                        public_key: public_key.clone(),
                        name: name.clone(),
                    });
                }
            }

            // Prepare the params vector from the structured parameters
            let params: Vec<&(dyn ToSql + Sync)> = structured_params_for_reg
                .iter()
                .flat_map(|tuple| {
                    vec![
                        &tuple.fee_recipient,
                        &tuple.gas_limit as &(dyn ToSql + Sync),
                        &tuple.timestamp,
                        &tuple.public_key,
                        &tuple.signature,
                        &tuple.inserted_at,
                        &tuple.user_agent,
                    ]
                })
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from(
                "INSERT INTO validator_registrations (fee_recipient, gas_limit, timestamp, public_key, signature, inserted_at, user_agent) VALUES ",
            );
            let num_params_per_row = 7;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row)
                        .map(|n| format!("${}", row * num_params_per_row + n))
                        .collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (public_key) DO UPDATE SET fee_recipient = excluded.fee_recipient, gas_limit = excluded.gas_limit, timestamp = excluded.timestamp, signature = excluded.signature, inserted_at = excluded.inserted_at, user_agent = excluded.user_agent, active = true");

            // Execute the query
            transaction.execute(&sql, &params[..]).await?;

            let params: Vec<&(dyn ToSql + Sync)> = structured_params_for_pref
                .iter()
                .flat_map(|tuple| {
                    vec![
                        &tuple.public_key as &(dyn ToSql + Sync),
                        &tuple.filtering,
                        &tuple.trusted_builders,
                        &tuple.header_delay,
                        &tuple.disable_inclusion_lists,
                    ]
                })
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from(
                "INSERT INTO validator_preferences (public_key, filtering, trusted_builders, header_delay, disable_inclusion_lists) VALUES ",
            );
            let num_params_per_row = 5;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row)
                        .map(|n| format!("${}", row * num_params_per_row + n))
                        .collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(
                " ON CONFLICT (public_key) DO UPDATE SET 
                            filtering = excluded.filtering, 
                            trusted_builders = excluded.trusted_builders, 
                            header_delay = excluded.header_delay,
                            disable_inclusion_lists = excluded.disable_inclusion_lists
                        WHERE validator_preferences.manual_override = FALSE",
            );

            // Execute the query
            transaction.execute(&sql, &params[..]).await?;

            if structured_params_for_trusted.is_empty() {
                transaction.commit().await?;
                continue;
            }

            let params: Vec<&(dyn ToSql + Sync)> = structured_params_for_trusted
                .iter()
                .flat_map(|tuple| vec![&tuple.public_key as &(dyn ToSql + Sync), &tuple.name])
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO trusted_proposers (pub_key, name) VALUES ");
            let num_params_per_row = 2;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row)
                        .map(|n| format!("${}", row * num_params_per_row + n))
                        .collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (pub_key) DO NOTHING");

            // Execute the query
            transaction.execute(&sql, &params[..]).await?;

            transaction.commit().await?;
        }

        record.record_success();
        Ok(())
    }

    async fn _flush_block_submissions(
        &self,
        batch: &[PendingBlockSubmissionValue],
        last_processed_slot: &mut i32,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("flush_block_submissions");
        let client = self.pool.get().await?;

        const CHUNK_SIZE: usize = 500;

        // chunck writes to avoid too many params in a single query and cause value too large to
        // transmit error
        for chunk in batch.chunks(CHUNK_SIZE) {
            // Step 1: build structured params for block_submission
            struct BlockParams<'a> {
                block_number: i32,
                slot_number: i32,
                parent_hash: &'a [u8],
                block_hash: &'a [u8],
                builder_pubkey: &'a [u8],
                proposer_pubkey: &'a [u8],
                proposer_fee_recipient: &'a [u8],
                gas_limit: i32,
                gas_used: i32,
                value: PostgresNumeric,
                num_txs: i32,
                timestamp: i64,
                first_seen: i64,
                region_id: i16,
                optimistic_version: i16,
                metadata: Option<&'a str>,
                is_adjusted: bool,
            }

            let mut structured_blocks: Vec<BlockParams> = Vec::with_capacity(chunk.len());
            for item in chunk {
                structured_blocks.push(BlockParams {
                    block_number: item.submission.block_number() as i32,
                    slot_number: item.submission.slot().as_u64() as i32,
                    parent_hash: item.submission.parent_hash().as_slice(),
                    block_hash: item.submission.block_hash().as_slice(),
                    builder_pubkey: item.submission.builder_public_key().as_slice(),
                    proposer_pubkey: item.submission.proposer_public_key().as_slice(),
                    proposer_fee_recipient: item.submission.proposer_fee_recipient().as_slice(),
                    gas_limit: item.submission.gas_limit() as i32,
                    gas_used: item.submission.gas_used() as i32,
                    value: PostgresNumeric::from(*item.submission.value()),
                    num_txs: item.submission.num_txs() as i32,
                    timestamp: item.submission.timestamp() as i64,
                    first_seen: item.trace.receive as i64,
                    region_id: self.region,
                    optimistic_version: item.optimistic_version as i16,
                    metadata: item.trace.metadata.as_deref(),
                    is_adjusted: item.is_adjusted,
                });
            }

            // Flatten into SQL params
            let mut params: Vec<&(dyn ToSql + Sync)> =
                Vec::with_capacity(structured_blocks.len() * BLOCK_SUBMISSION_FIELD_COUNT);
            for blk in &structured_blocks {
                params.push(&blk.block_number);
                params.push(&blk.slot_number);
                params.push(&blk.parent_hash);
                params.push(&blk.block_hash);
                params.push(&blk.builder_pubkey);
                params.push(&blk.proposer_pubkey);
                params.push(&blk.proposer_fee_recipient);
                params.push(&blk.gas_limit);
                params.push(&blk.gas_used);
                params.push(&blk.value);
                params.push(&blk.num_txs);
                params.push(&blk.timestamp);
                params.push(&blk.first_seen);
                params.push(&blk.region_id);
                params.push(&blk.optimistic_version);
                params.push(&blk.metadata);
                params.push(&blk.is_adjusted);
            }

            // Build and execute INSERT for this chunk
            let num_cols = BLOCK_SUBMISSION_FIELD_COUNT;
            let mut sql = String::from(
                "INSERT INTO block_submission (block_number, slot_number, parent_hash, block_hash, builder_pubkey, proposer_pubkey, proposer_fee_recipient, gas_limit, gas_used, value, num_txs, timestamp, first_seen, region_id, optimistic_version, metadata, is_adjusted) VALUES ",
            );
            let clauses: Vec<String> = (0..structured_blocks.len())
                .map(|i| {
                    let start = i * num_cols + 1;
                    let placeholders: Vec<String> =
                        (0..num_cols).map(|j| format!("${}", start + j)).collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();
            sql.push_str(&clauses.join(", "));
            client.execute(&sql, &params).await?;
        }

        // Step 2: build structured params for slot_preferences (process entire batch)
        let mut new_rows: Vec<(i32, Vec<u8>)> = Vec::with_capacity(batch.len());
        let mut tmp_last_processed_slot = *last_processed_slot;
        for item in batch {
            let slot = item.submission.slot().as_u64() as i32;
            if slot > tmp_last_processed_slot {
                tmp_last_processed_slot = slot;
                new_rows.push((slot, item.submission.proposer_public_key().as_slice().to_vec()));
            }
        }
        if !new_rows.is_empty() {
            // Flatten into SQL params
            let mut sp_params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(new_rows.len() * 2);
            for (slot, pk) in &new_rows {
                sp_params.push(slot);
                sp_params.push(pk);
            }

            // Build and execute a single INSERT...SELECT
            let mut vp_sql = String::from(
                "INSERT INTO slot_preferences (slot_number, proposer_pubkey, filtering, trusted_builders, header_delay) ",
            );
            vp_sql.push_str("SELECT r.slot_number, r.proposer_pubkey, vp.filtering, vp.trusted_builders, vp.header_delay FROM (VALUES ");
            let val_clauses: Vec<String> = (0..new_rows.len())
                .map(|i| {
                    let start = i * 2 + 1;
                    format!("(${}::INTEGER, ${}::BYTEA)", start, start + 1)
                })
                .collect();
            vp_sql.push_str(&val_clauses.join(", "));
            vp_sql.push_str(") AS r(slot_number, proposer_pubkey) ");
            vp_sql.push_str("JOIN validator_preferences vp ON vp.public_key = r.proposer_pubkey ");
            vp_sql.push_str("ON CONFLICT (slot_number) DO NOTHING");
            client.execute(&vp_sql, &sp_params).await?;
        }

        *last_processed_slot = tmp_last_processed_slot;
        record.record_success();
        Ok(())
    }
}

impl Default for PostgresDatabaseService {
    fn default() -> Self {
        let mut cfg = Config::new();
        cfg.host = Some("localhost".to_string());
        cfg.port = Some(5432);
        cfg.dbname = Some("postgres".to_string());
        cfg.user = Some("postgres".to_string());
        cfg.password = Some("password".to_string());
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });

        let pool = cfg.create_pool(None, NoTls).unwrap();
        let high_priority_pool = cfg.create_pool(None, NoTls).unwrap();

        PostgresDatabaseService {
            region: 1,
            pool: Arc::new(pool),
            high_priority_pool: Arc::new(high_priority_pool),
            local_cache: Arc::new(LocalCache::new()),
        }
    }
}

impl PostgresDatabaseService {
    #[instrument(skip_all)]
    pub async fn get_validator_registration(
        &self,
        pub_key: &BlsPublicKeyBytes,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError> {
        let mut record = DbMetricRecord::new("get_validator_registration");

        match self
            .pool
            .get()
            .await?
            .query(
                "
                SELECT
                    validator_registrations.fee_recipient,
                    validator_registrations.gas_limit,
                    validator_registrations.timestamp,
                    validator_registrations.public_key,
                    validator_registrations.signature,
                    validator_preferences.filtering,
                    validator_preferences.trusted_builders,
                    validator_preferences.header_delay,
                    validator_preferences.disable_inclusion_lists,
                    validator_registrations.inserted_at,
                    validator_registrations.user_agent,
                    validator_preferences.delay_ms
                FROM validator_registrations
                INNER JOIN validator_preferences ON validator_registrations.public_key = validator_preferences.public_key
                WHERE validator_registrations.public_key = $1 AND validator_registrations.active = true
            ",
                &[&(pub_key.as_slice())],
            )
            .await?
        {
            rows if rows.is_empty() => Err(DatabaseError::ValidatorRegistrationNotFound),
            rows => {
                record.record_success();
                parse_row(rows.first().unwrap())
            },
        }
    }

    #[instrument(skip_all)]
    async fn get_validator_registrations(
        &self,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_validator_registrations");

        let rows = self
            .pool
            .get()
            .await?
            .query(
                "
                    SELECT * FROM validator_registrations
                    INNER JOIN validator_preferences
                    ON validator_registrations.public_key = validator_preferences.public_key
                    WHERE validator_registrations.active = true
                ",
                &[],
            )
            .await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: &[&BlsPublicKeyBytes],
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_validator_registrations_for_pub_keys");

        let client = self.high_priority_pool.get().await.map_err(DatabaseError::from)?;

        // Constructing the query
        let placeholders: Vec<String> = (1..=pub_keys.len()).map(|i| format!("${i}")).collect();
        let query = format!(
            "SELECT *
            FROM validator_registrations
            INNER JOIN validator_preferences ON validator_registrations.public_key = validator_preferences.public_key
            WHERE validator_registrations.active = true AND validator_preferences.public_key IN ({})",
            placeholders.join(", ")
        );

        // Preparing the query
        let stmt = client.prepare(&query).await.map_err(DatabaseError::from)?;

        let params: Vec<Box<dyn ToSql + Sync + Send>> = pub_keys
            .iter()
            .map(|key| Box::new(key.as_slice()) as Box<dyn ToSql + Sync + Send>)
            .collect();

        let params_slice: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();

        let rows = client.query(&stmt, &params_slice).await.map_err(DatabaseError::from)?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn set_proposer_duties(
        &self,
        proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("set_proposer_duties");

        let mut client = self.high_priority_pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                INSERT INTO proposer_duties_archive SELECT * FROM proposer_duties order by slot_number ON CONFLICT (slot_number) DO UPDATE SET public_key = excluded.public_key, validator_index = excluded.validator_index;
            ",
                &[],
            )
            .await?;

        transaction
            .execute(
                "
                DELETE FROM proposer_duties;
            ",
                &[],
            )
            .await?;

        let mut structured_params: Vec<(i32, i32, Vec<u8>)> =
            Vec::with_capacity(proposer_duties.len());
        for entry in proposer_duties.iter() {
            structured_params.push((
                entry.slot.as_u64() as i32,
                entry.validator_index as i32,
                entry.entry.registration.message.pubkey.to_vec(),
            ));
        }

        // Prepare the params vector from the structured parameters
        let params: Vec<&(dyn ToSql + Sync)> = structured_params
            .iter()
            .flat_map(|tuple| vec![&tuple.0, &tuple.1, &tuple.2 as &(dyn ToSql + Sync)])
            .collect();

        // Construct the SQL statement with multiple VALUES clauses
        let mut sql = String::from(
            "INSERT INTO proposer_duties (slot_number, validator_index, public_key) VALUES ",
        );
        let num_params_per_row = 3;
        let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
            .map(|row| {
                let placeholders: Vec<String> = (1..=num_params_per_row)
                    .map(|n| format!("${}", row * num_params_per_row + n))
                    .collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();

        // Join the values clauses and append them to the SQL statement
        sql.push_str(&values_clauses.join(", "));
        sql.push_str(" ON CONFLICT (slot_number) DO NOTHING");

        // Execute the query
        transaction.execute(&sql, &params[..]).await?;

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    pub async fn set_known_validators(
        &self,
        known_validators: Vec<ValidatorSummary>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("set_known_validators");

        let new_keys_set: FxHashSet<BlsPublicKeyBytes> =
            known_validators.iter().map(|validator| validator.validator.pubkey).collect();

        let keys_to_add: Vec<BlsPublicKeyBytes>;
        let keys_to_remove: Vec<BlsPublicKeyBytes>;

        {
            let mut curr_known_guard = self.local_cache.known_validators_cache.write();
            info!("Known validators: current cache size: {:?}", curr_known_guard.len());

            keys_to_add = new_keys_set.difference(&curr_known_guard).cloned().collect();
            keys_to_remove = curr_known_guard.difference(&new_keys_set).cloned().collect();

            info!("Known validators: added: {:?}", keys_to_add.len());
            info!("Known validators: removed: {:?}", keys_to_add.len());

            for key in &keys_to_add {
                curr_known_guard.insert(*key);
            }
            for key in &keys_to_remove {
                curr_known_guard.remove(key);
            }

            info!("Known validators: updated cache size: {:?}", curr_known_guard.len());
        }

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        // Perform batch deletion
        for chunk in keys_to_remove.chunks(10000) {
            let sql = "DELETE FROM known_validators WHERE public_key = ANY($1::bytea[])";
            let byte_keys: Vec<Vec<u8>> = chunk.iter().map(|k| k.to_vec()).collect();
            transaction.execute(sql, &[&byte_keys]).await?;
        }

        // Perform batch insertion
        for chunk in keys_to_add.chunks(10000) {
            let mut sql = String::from("INSERT INTO known_validators (public_key) VALUES ");
            let values_clauses: Vec<String> =
                (1..=chunk.len()).map(|i| format!("(${i})")).collect();

            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (public_key) DO NOTHING");

            let mut structured_params: Vec<Vec<u8>> = Vec::new();
            for validator in chunk.iter() {
                structured_params.push(validator.to_vec());
            }

            let params: Vec<&(dyn ToSql + Sync)> =
                structured_params.iter().flat_map(|v| vec![v as &(dyn ToSql + Sync)]).collect();

            transaction.execute(&sql, &params[..]).await?;
        }

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn load_validator_pools(&self) {
        let mut record = DbMetricRecord::new("load_validator_pools");

        let client = match self.high_priority_pool.get().await {
            Ok(client) => client,
            Err(e) => {
                error!("Error getting client from pool: {}", e);
                return;
            }
        };
        let rows = match client.query("SELECT * FROM validator_pools", &[]).await {
            Ok(rows) => rows,
            Err(e) => {
                error!("Error querying validator_pools: {}", e);
                return;
            }
        };

        for row in rows {
            let api_key: String = row.get::<&str, &str>("api_key").to_string();
            let name: String = row.get::<&str, &str>("name").to_string();
            self.local_cache.validator_pool_cache.insert(api_key, name);
        }

        record.record_success();
    }

    #[instrument(skip_all)]
    pub async fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        payload_hash: &B256,
        message_received: u64,
        payload_fetched: u64,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_too_late_get_payload");

        let region_id = self.region;
        self.pool
            .get()
            .await?
            .execute(
                "
                    INSERT INTO late_payload
                        (block_hash, slot_number, region_id, proposer_pubkey, message_received, payload_fetched)
                    VALUES 
                        ($1, $2, $3, $4, $5, $6)
                ",
                &[
                    &(payload_hash.as_slice()),
                    &(slot as i32),
                    &(region_id),
                    &(proposer_pub_key.to_vec()),
                    &(message_received as i64),
                    &(payload_fetched as i64),
                ],
            )
            .await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn save_delivered_payload(
        &self,
        save_payload_params: &SavePayloadParams,
    ) -> Result<(), DatabaseError> {
        save_payload_params.latency_trace.record_metrics();
        let mut record = DbMetricRecord::new("save_delivered_payload");

        let region_id = self.region;
        let block_hash = save_payload_params.payload.execution_payload.block_hash;
        let client = self.pool.get().await?;
        client.execute(
            "
                INSERT INTO delivered_payload 
                    (block_hash, payload_parent_hash, fee_recipient, state_root, receipts_root, logs_bloom, prev_randao, timestamp, block_number, gas_limit, gas_used, extra_data, base_fee_per_gas, user_agent, slot_number, builder_pubkey, proposer_pubkey, proposer_fee_recipient, value, num_txs, filtering )
                VALUES 
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
                ON CONFLICT (block_hash)
                DO NOTHING
            ",
            &[
                &(block_hash.as_slice()),
                &(save_payload_params.payload.execution_payload.parent_hash.as_slice()),
                &(save_payload_params.payload.execution_payload.fee_recipient.as_slice()),
                &(save_payload_params.payload.execution_payload.state_root.as_slice()),
                &(save_payload_params.payload.execution_payload.receipts_root.as_slice()),
                &(save_payload_params.payload.execution_payload.logs_bloom.to_vec()),
                &(save_payload_params.payload.execution_payload.prev_randao.as_slice()),
                &(save_payload_params.payload.execution_payload.timestamp as i64),
                &(save_payload_params.payload.execution_payload.block_number as i32),
                &(save_payload_params.payload.execution_payload.gas_limit as i32),
                &(save_payload_params.payload.execution_payload.gas_used as i32),
                &(save_payload_params.payload.execution_payload.extra_data.to_vec()),
                &(PostgresNumeric::from(save_payload_params.payload.execution_payload.base_fee_per_gas)),
                &(save_payload_params.user_agent),
                &(save_payload_params.slot as i32),
                &(save_payload_params.builder_pub_key.as_slice()),
                &(save_payload_params.proposer_pub_key.as_slice()),
                &(save_payload_params.proposer_fee_recipient.as_slice()),
                &(PostgresNumeric::from(save_payload_params.value)),
                &(save_payload_params.payload.execution_payload.transactions.len() as i32),
                &(save_payload_params.filtering as i16),
            ],
            ).await?;

        client.execute(
                "
                    INSERT INTO delivered_payload_preferences (block_hash, filtering, trusted_builders)
                    SELECT $1::bytea, filtering, trusted_builders
                    FROM validator_preferences
                    WHERE public_key = $2::bytea
                    ON CONFLICT (block_hash) DO NOTHING;                
                ",
                &[
                    &(block_hash.as_slice()),
                    &(save_payload_params.proposer_pub_key.as_slice()),
                ],
                ).await?;

        client.execute(
            "
                INSERT INTO payload_trace
                    (block_hash, region_id, receive, proposer_index_validated, signature_validated, payload_fetched, validation_complete, beacon_client_broadcast, broadcaster_block_broadcast, on_deliver_payload)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ",
            &[
                &(block_hash.as_slice()),
                &(region_id),
                &(save_payload_params.latency_trace.receive as i64),
                &(save_payload_params.latency_trace.proposer_index_validated as i64),
                &(save_payload_params.latency_trace.signature_validated as i64),
                &(save_payload_params.latency_trace.payload_fetched as i64),
                &(save_payload_params.latency_trace.validation_complete as i64),
                &(save_payload_params.latency_trace.beacon_client_broadcast as i64),
                &(save_payload_params.latency_trace.broadcaster_block_broadcast as i64),
                &(save_payload_params.latency_trace.on_deliver_payload as i64),
            ],
        ).await?;

        if !save_payload_params.payload.execution_payload.transactions.is_empty() {
            // Save the transactions
            let mut structured_params: Vec<(Vec<u8>, &[u8])> = Vec::new();
            for entry in save_payload_params.payload.execution_payload.transactions.iter() {
                structured_params.push((
                    save_payload_params.payload.execution_payload.block_hash.to_vec(),
                    entry.as_ref(),
                ));
            }

            // Prepare the params vector from the structured parameters
            let params: Vec<&(dyn ToSql + Sync)> = structured_params
                .iter()
                .flat_map(|tuple| {
                    vec![&tuple.0 as &(dyn ToSql + Sync), &tuple.1 as &(dyn ToSql + Sync)]
                })
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO transaction (block_hash, bytes) VALUES ");
            let num_params_per_row = 2;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row)
                        .map(|n| format!("${}", row * num_params_per_row + n))
                        .collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (md5(block_hash::text), md5(bytes::text)) DO NOTHING");

            client.execute(&sql, &params[..]).await?;
        }

        if !save_payload_params.payload.execution_payload.withdrawals.is_empty() {
            // Save the withdrawals
            #[allow(clippy::type_complexity)]
            let mut structured_params: Vec<(i32, Vec<u8>, i32, &[u8], i64)> = Vec::new();
            for entry in save_payload_params.payload.execution_payload.withdrawals.iter() {
                structured_params.push((
                    entry.index as i32,
                    save_payload_params.payload.execution_payload.block_hash.to_vec(),
                    entry.validator_index as i32,
                    entry.address.as_ref(),
                    entry.amount as i64,
                ));
            }

            // Prepare the params vector from the structured parameters
            let params: Vec<&(dyn ToSql + Sync)> = structured_params
                .iter()
                .flat_map(|tuple| {
                    vec![
                        &tuple.0,
                        &tuple.1 as &(dyn ToSql + Sync),
                        &tuple.2,
                        &tuple.3 as &(dyn ToSql + Sync),
                        &tuple.4,
                    ]
                })
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from(
                "INSERT INTO withdrawal (index, block_hash, validator_index, address, amount) VALUES ",
            );
            let num_params_per_row = 5;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row)
                        .map(|n| format!("${}", row * num_params_per_row + n))
                        .collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (index, block_hash) DO NOTHING");

            client.execute(&sql, &params[..]).await?;
        }

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn store_builders_info(
        &self,
        builders: &[BuilderInfoDocument],
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("store_builders_info");

        if builders.is_empty() {
            record.record_success();
            return Ok(());
        }

        // Define the number of SQL fields per builder row
        const FIELD_COUNT: usize = 6;

        // Step 1: materialize all temp values into a stable struct
        struct BuilderParams {
            pubkey: Vec<u8>,
            collateral: PostgresNumeric,
            is_optimistic: bool,
            is_optimistic_for_regional_filtering: bool,
            builder_id: Option<String>,
            builder_ids: Option<Vec<String>>,
        }

        let mut structured_builders = Vec::with_capacity(builders.len());
        for builder in builders {
            structured_builders.push(BuilderParams {
                pubkey: builder.pub_key.as_slice().to_vec(),
                collateral: PostgresNumeric::from(builder.builder_info.collateral),
                is_optimistic: builder.builder_info.is_optimistic,
                is_optimistic_for_regional_filtering: builder
                    .builder_info
                    .is_optimistic_for_regional_filtering,
                builder_id: builder.builder_info.builder_id.clone(),
                builder_ids: builder.builder_info.builder_ids.clone(),
            });
        }

        // Step 2: flatten into SQL param list
        let mut values: Vec<&(dyn ToSql + Sync)> =
            Vec::with_capacity(structured_builders.len() * FIELD_COUNT);
        for b in &structured_builders {
            values.push(&b.pubkey);
            values.push(&b.collateral);
            values.push(&b.is_optimistic);
            values.push(&b.is_optimistic_for_regional_filtering);
            values.push(&b.builder_id);
            values.push(&b.builder_ids);
        }

        // Step 3: generate placeholder string
        let rows: Vec<String> = (0..structured_builders.len())
            .map(|i| {
                let start = i * FIELD_COUNT + 1;
                let placeholders: Vec<String> =
                    (0..FIELD_COUNT).map(|j| format!("${}", start + j)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();

        let sql = format!(
            "
            INSERT INTO builder_info (
                public_key,
                collateral,
                is_optimistic,
                is_optimistic_for_regional_filtering,
                builder_id,
                builder_ids
            ) VALUES
            {}
            ON CONFLICT (public_key)
            DO UPDATE SET
                builder_ids = array_concat_uniq(
                    COALESCE(builder_info.builder_ids, '{{}}'::character varying[]),
                    EXCLUDED.builder_ids
                )
            ",
            rows.join(", ")
        );

        self.pool.get().await?.execute(&sql, &values).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_all_builder_infos");

        let rows =
            self.high_priority_pool.get().await?.query("SELECT * FROM builder_info", &[]).await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn db_demote_builder(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        reason: String,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("db_demote_builder");

        let mut client = self.high_priority_pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                    UPDATE builder_info 
                    SET is_optimistic = FALSE 
                    WHERE public_key = $1
                ",
                &[&(builder_pub_key.as_slice())],
            )
            .await?;

        let timestamp = utcnow_ms();
        transaction
            .execute(
                "
                    INSERT INTO demotions (public_key, block_hash, demotion_time, reason, slot_number)
                    VALUES ($1, $2, $3, $4, $5)
                ",
                &[
                    &(builder_pub_key.as_slice()),
                    &(block_hash.as_slice()),
                    &(timestamp as i64),
                    &(reason),
                    &(slot  as i32),
                ],
            )
            .await?;

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn get_bids(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_bids");

        let filters = PgBidFilters::from(filters);

        // Build 2 queries - one for bids that have entries in the `block_submission` table
        // and possibly also in the `header_submission` table (v1 and v2 submissions) and a second
        // for bids that are only present in the `header_submission` table (v3 submissions).
        let mut block_query = String::from("
            SELECT
                block_submission.block_number block_number,
                block_submission.slot_number slot_number,
                block_submission.parent_hash,
                block_submission.block_hash,
                block_submission.builder_pubkey builder_public_key,
                block_submission.proposer_pubkey proposer_public_key,
                block_submission.proposer_fee_recipient proposer_fee_recipient,
                block_submission.gas_limit gas_limit,
                block_submission.gas_used gas_used,
                block_submission.value submission_value,
                block_submission.num_txs num_txs,
                LEAST(block_submission.first_seen, header_submission.first_seen) submission_timestamp
            FROM 
                block_submission
            LEFT JOIN
                header_submission ON block_submission.block_hash = header_submission.block_hash
        ");

        let mut header_query = String::from(
            "
            SELECT
                header_submission.block_number block_number,
                header_submission.slot_number slot_number,
                header_submission.parent_hash,
                header_submission.block_hash,
                header_submission.builder_pubkey builder_public_key,
                header_submission.proposer_pubkey proposer_public_key,
                header_submission.proposer_fee_recipient proposer_fee_recipient,
                header_submission.gas_limit gas_limit,
                header_submission.gas_used gas_used,
                header_submission.value submission_value,
                header_submission.tx_count num_txs,
                header_submission.first_seen submission_timestamp
            FROM 
                header_submission
        ",
        );

        let filtering = match validator_preferences.filtering {
            Filtering::Regional => Some(1_i16),
            Filtering::Global => None,
        };

        if filtering.is_some() {
            block_query.push_str(
                "
                LEFT JOIN
                    slot_preferences
                ON
                    block_submission.slot_number = slot_preferences.slot_number
            ",
            );
            header_query.push_str(
                "
                LEFT JOIN
                    slot_preferences
                ON
                    header_submission.slot_number = slot_preferences.slot_number
            ",
            );
        }

        block_query.push_str(" WHERE 1 = 1");
        header_query.push_str(" WHERE header_submission.tx_count IS NOT NULL");

        let mut param_index = 1;
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        if let Some(slot) = filters.slot() {
            block_query.push_str(&format!(" AND block_submission.slot_number = ${param_index}"));
            header_query.push_str(&format!(" AND header_submission.slot_number = ${param_index}"));
            params.push(Box::new(slot));
            param_index += 1;
        }

        if let Some(block_number) = filters.block_number() {
            block_query.push_str(&format!(" AND block_submission.block_number = ${param_index}"));
            header_query.push_str(&format!(" AND header_submission.block_number = ${param_index}"));
            params.push(Box::new(block_number));
            param_index += 1;
        }

        if let Some(proposer_pubkey) = filters.proposer_pubkey() {
            block_query
                .push_str(&format!(" AND block_submission.proposer_pubkey = ${param_index}"));
            header_query
                .push_str(&format!(" AND header_submission.proposer_pubkey = ${param_index}"));
            params.push(Box::new(proposer_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(builder_pubkey) = filters.builder_pubkey() {
            block_query.push_str(&format!(" AND block_submission.builder_pubkey = ${param_index}"));
            header_query
                .push_str(&format!(" AND header_submission.builder_pubkey = ${param_index}"));
            params.push(Box::new(builder_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(block_hash) = filters.block_hash() {
            block_query.push_str(&format!(" AND block_submission.block_hash = ${param_index}"));
            header_query.push_str(&format!(" AND header_submission.block_hash = ${param_index}"));
            params.push(Box::new(block_hash));
            param_index += 1;
        }

        if let Some(filtering) = filtering {
            block_query.push_str(&format!(
                " AND (slot_preferences.filtering = ${param_index} OR slot_preferences.filtering IS NULL)"
            ));
            header_query.push_str(&format!(
                " AND (slot_preferences.filtering = ${param_index} OR slot_preferences.filtering IS NULL)"
            ));
            params.push(Box::new(filtering));
            param_index += 1;
        }

        let mut query = format!("{block_query} UNION {header_query}");
        if let Some(limit) = filters.limit() {
            params.push(Box::new(limit));
            query.push_str(&format!(" LIMIT ${}", param_index));
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        let rows = self.pool.get().await?.query(&query, &params_refs[..]).await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let mut mig_slot = DELIVERED_PAYLOADS_MIG_SLOT.load(Ordering::Relaxed);
        if mig_slot == 0 {
            let row = self.pool.get().await?
                .query_one(
                    "SELECT COALESCE(MIN(slot_number), 0) as min_slot FROM delivered_payload WHERE slot_number IS NOT NULL", 
                    &[]
                )
                .await?;
            let slot_num: i32 = row.get("min_slot");
            mig_slot = slot_num as u64;
            DELIVERED_PAYLOADS_MIG_SLOT.store(mig_slot, Ordering::Relaxed);
            info!("Loaded delivered_payload migration slot: {}", mig_slot);
        }

        let use_legacy = if let Some(requested_slot) = filters.slot {
            requested_slot < mig_slot
        } else if let Some(cursor) = filters.cursor {
            match filters.limit {
                Some(limit) => cursor.saturating_sub(limit) < mig_slot,
                None => true,
            }
        } else {
            false
        };

        if use_legacy {
            self.get_delivered_payloads_legacy(filters, validator_preferences).await
        } else {
            self.get_delivered_payloads_new(filters, validator_preferences).await
        }
    }

    #[instrument(skip_all)]
    pub async fn get_delivered_payloads_new(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_delivered_payloads");

        let filters = PgBidFilters::from(filters);
        let mut query = String::from(
            "
            SELECT
                delivered_payload.slot_number            slot_number,
                delivered_payload.payload_parent_hash    parent_hash,
                delivered_payload.block_hash             block_hash,
                delivered_payload.builder_pubkey         builder_public_key,
                delivered_payload.proposer_pubkey        proposer_public_key,
                delivered_payload.proposer_fee_recipient proposer_fee_recipient,
                delivered_payload.value                  submission_value,
                delivered_payload.gas_limit              gas_limit,
                delivered_payload.gas_used               gas_used,
                delivered_payload.block_number           block_number,
                delivered_payload.num_txs                num_txs
            FROM
                delivered_payload
        ",
        );

        let filtering = match validator_preferences.filtering {
            Filtering::Regional => Some(1_i16),
            Filtering::Global => None,
        };

        query.push_str(" WHERE 1 = 1 AND delivered_payload.slot_number IS NOT NULL");

        let mut param_index = 1;
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        if let Some(slot) = filters.slot() {
            query.push_str(&format!(" AND delivered_payload.slot_number = ${param_index}"));
            params.push(Box::new(slot));
            param_index += 1;
        }

        if let Some(cursor) = filters.cursor() {
            query.push_str(&format!(" AND delivered_payload.slot_number <= ${param_index}"));
            params.push(Box::new(cursor));
            param_index += 1;
        }

        if let Some(block_number) = filters.block_number() {
            query.push_str(&format!(" AND delivered_payload.block_number = ${param_index}"));
            params.push(Box::new(block_number));
            param_index += 1;
        }

        if let Some(proposer_pubkey) = filters.proposer_pubkey() {
            query.push_str(&format!(" AND delivered_payload.proposer_pubkey = ${param_index}"));
            params.push(Box::new(proposer_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(builder_pubkey) = filters.builder_pubkey() {
            query.push_str(&format!(" AND delivered_payload.builder_pubkey = ${param_index}"));
            params.push(Box::new(builder_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(block_hash) = filters.block_hash() {
            query.push_str(&format!(" AND delivered_payload.block_hash = ${param_index}"));
            params.push(Box::new(block_hash));
            param_index += 1;
        }

        if let Some(filtering) = filtering {
            query.push_str(&format!(" AND delivered_payload.filtering = ${param_index}"));
            params.push(Box::new(filtering));
            param_index += 1;
        }

        if let Some(order) = filters.order() {
            query.push_str(" ORDER BY delivered_payload.value ");
            query.push_str(if order >= 0 { "ASC" } else { "DESC" });
        } else {
            query.push_str(" ORDER BY delivered_payload.slot_number DESC");
        }

        if let Some(limit) = filters.limit() {
            query.push_str(&format!(" LIMIT ${param_index}"));
            params.push(Box::new(limit));
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        let rows = self.pool.get().await?.query(&query, &params_refs[..]).await?;
        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn get_delivered_payloads_legacy(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_delivered_payloads_legacy");

        let filters = PgBidFilters::from(filters);
        let mut query = String::from(
            "
            SELECT
                block_submission.slot_number            slot_number,
                block_submission.parent_hash            parent_hash,
                block_submission.block_hash             block_hash,
                block_submission.builder_pubkey         builder_public_key,
                block_submission.proposer_pubkey        proposer_public_key,
                block_submission.proposer_fee_recipient proposer_fee_recipient,
                block_submission.value                  submission_value,
                block_submission.gas_limit              gas_limit,
                block_submission.gas_used               gas_used,
                block_submission.block_number           block_number,
                block_submission.num_txs                num_txs
            FROM
                block_submission
            INNER JOIN
                delivered_payload ON block_submission.block_number = delivered_payload.block_number and block_submission.block_hash = delivered_payload.block_hash
        ",
        );

        let filtering = match validator_preferences.filtering {
            Filtering::Regional => Some(1_i16),
            Filtering::Global => None,
        };

        if filtering.is_some() {
            query.push_str(
                "
                INNER JOIN
                    delivered_payload_preferences
                ON
                    delivered_payload.block_hash = delivered_payload_preferences.block_hash
            ",
            );
        }

        query.push_str(" WHERE 1 = 1");

        let mut param_index = 1;
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        if let Some(slot) = filters.slot() {
            query.push_str(&format!(" AND block_submission.slot_number = ${param_index}"));
            params.push(Box::new(slot));
            param_index += 1;
        }

        if let Some(cursor) = filters.cursor() {
            query.push_str(&format!(" AND block_submission.slot_number <= ${param_index}"));
            params.push(Box::new(cursor));
            param_index += 1;
        }

        if let Some(block_number) = filters.block_number() {
            query.push_str(&format!(" AND block_submission.block_number = ${param_index}"));
            params.push(Box::new(block_number));
            param_index += 1;
        }

        if let Some(proposer_pubkey) = filters.proposer_pubkey() {
            query.push_str(&format!(" AND block_submission.proposer_pubkey = ${param_index}"));
            params.push(Box::new(proposer_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(builder_pubkey) = filters.builder_pubkey() {
            query.push_str(&format!(" AND block_submission.builder_pubkey = ${param_index}"));
            params.push(Box::new(builder_pubkey.to_vec()));
            param_index += 1;
        }

        if let Some(block_hash) = filters.block_hash() {
            query.push_str(&format!(" AND block_submission.block_hash = ${param_index}"));
            params.push(Box::new(block_hash));
            param_index += 1;
        }

        if let Some(filtering) = filtering {
            query.push_str(&format!(
                " AND delivered_payload_preferences.filtering = ${param_index}"
            ));
            params.push(Box::new(filtering));
            param_index += 1;
        }

        if let Some(order) = filters.order() {
            query.push_str(" ORDER BY block_submission.value ");
            query.push_str(if order >= 0 { "ASC" } else { "DESC" });
        } else {
            query.push_str(" ORDER BY block_submission.slot_number DESC");
        }

        if let Some(limit) = filters.limit() {
            query.push_str(&format!(" LIMIT ${param_index}"));
            params.push(Box::new(limit));
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        let rows = self.pool.get().await?.query(&query, &params_refs[..]).await?;
        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn save_get_header_call(
        &self,
        params: GetHeaderParams,
        best_block_hash: B256,
        value: U256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_get_header_call");

        let region_id = self.region;

        let client = self.pool.get().await?;

        client
            .execute(
                "
                    INSERT INTO get_header
                        (slot_number, region_id, parent_hash, proposer_pubkey, block_hash, value, mev_boost, user_agent, receive, validation_complete, best_bid_fetched)
                    VALUES
                        ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ",
                &[
                    &(params.slot as i32),
                    &(region_id),
                    &(params.parent_hash.as_slice()),
                    &(params.pubkey.as_slice()),
                    &(best_block_hash.as_slice()),
                    &(PostgresNumeric::from(value)),
                    &(mev_boost),
                    &(user_agent),
                    &(trace.receive as i64),
                    &(trace.validation_complete as i64),
                    &(trace.best_bid_fetched as i64),
                ],
            )
            .await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn save_failed_get_payload(
        &self,
        slot: u64,
        block_hash: B256,
        error: String,
        trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        trace.record_metrics();
        let mut record = DbMetricRecord::new("save_failed_get_payload");

        let region_id = self.region;

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        let block_hash_bytes: &[u8] = block_hash.as_ref();

        transaction
            .execute(
                "
                    INSERT INTO failed_payload
                        (region_id, slot_number, block_hash, error)
                    VALUES
                        ($1, $2, $3, $4)
                ",
                &[&(region_id), &(slot as i32), &(block_hash_bytes), &(error)],
            )
            .await?;

        transaction.execute(
            "
                INSERT INTO payload_trace
                    (block_hash, region_id, receive, proposer_index_validated, signature_validated, payload_fetched, validation_complete, beacon_client_broadcast, broadcaster_block_broadcast, on_deliver_payload)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ",
            &[
                &(block_hash_bytes),
                &(region_id),
                &(trace.receive as i64),
                &(trace.proposer_index_validated as i64),
                &(trace.signature_validated as i64),
                &(trace.payload_fetched as i64),
                &(trace.validation_complete as i64),
                &(trace.beacon_client_broadcast as i64),
                &(trace.broadcaster_block_broadcast as i64),
                &(trace.on_deliver_payload as i64),
            ],
        ).await?;

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn save_gossiped_payload_trace(
        &self,
        block_hash: B256,
        trace: GossipedPayloadTrace,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_gossiped_payload_trace");

        let region_id = self.region;

        self.pool.get().await?.execute(
            "
                INSERT INTO
                    gossiped_payload_trace (block_hash, region_id, receive, pre_checks, auctioneer_update)
                VALUES
                    ($1, $2, $3, $4, $5)
            ",
            &[
                &(block_hash.as_slice()),
                &(region_id),
                &(trace.receive as i64),
                &(trace.pre_checks as i64),
                &(trace.auctioneer_update as i64),
            ],
        ).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_trusted_proposers");
        let rows = self
            .high_priority_pool
            .get()
            .await?
            .query(
                "
                SELECT * FROM trusted_proposers 
            ",
                &[],
            )
            .await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn save_inclusion_list(
        &self,
        inclusion_list: &InclusionListWithMetadata,
        slot: u64,
        block_parent_hash: &B256,
        proposer_pubkey: &BlsPublicKeyBytes,
    ) -> Result<(), Vec<DatabaseError>> {
        let mut record = DbMetricRecord::new("save_inclusion_list");
        let client = self.high_priority_pool.get().await.map_err(|err| vec![err.into()])?;
        let mut errors = vec![];

        for tx in &inclusion_list.txs {
            let result = client.execute(
                "
                    INSERT INTO
                        inclusion_list_txs (tx_hash, bytes, slot, block_parent_hash, proposer_pubkey)
                    VALUES
                        ($1, $2, $3, $4, $5)
                    ON CONFLICT (tx_hash)
                    DO UPDATE SET
                        slot = EXCLUDED.slot,
                        block_parent_hash = EXCLUDED.block_parent_hash, 
                        proposer_pubkey = EXCLUDED.proposer_pubkey
                ",
                &[
                    &(tx.hash.as_slice()),
                    &(tx.bytes.as_ref()),
                    &(slot as i64),
                    &(block_parent_hash.as_slice()),
                    &(alloy_primitives::hex::encode_prefixed(proposer_pubkey).as_bytes()),
                ],
            ).await;

            if let Err(err) = result {
                warn!(
                    head_slot = &slot,
                    "Error saving inclusion list in the 'inclusion_list_txs' table in postgres: {:?}",
                    err
                );
                errors.push(err.into());
                record.record_failure();
                continue;
            };
        }

        if errors.is_empty() {
            record.record_success();
            Ok(())
        } else {
            Err(errors)
        }
    }

    #[instrument(skip_all)]
    pub async fn get_block_adjustments_for_slot(
        &self,
        slot: Slot,
    ) -> Result<Vec<DataAdjustmentsResponse>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_block_adjustments_for_slot");

        let rows = self
            .pool
            .get()
            .await?
            .query(
                "
                SELECT
                    builder_pubkey,
                    block_number,
                    delta,
                    submitted_block_hash,
                    submitted_received_at,
                    submitted_value,
                    adjusted_block_hash,
                    adjusted_value
                FROM bid_adjustments
                WHERE slot = $1 AND COALESCE(is_dry_run, FALSE) = FALSE
                ",
                &[&(slot.as_u64() as i64)],
            )
            .await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    pub async fn save_block_adjustments_data(
        &self,
        entry: DataAdjustmentsEntry,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_block_adjustments_data");

        self.pool
            .get()
            .await?
            .execute(
                "
                INSERT INTO
                    bid_adjustments (
                        slot,
                        builder_pubkey,
                        block_number,
                        delta,
                        submitted_block_hash,
                        submitted_received_at,
                        submitted_value,
                        adjusted_block_hash,
                        adjusted_value,
                        is_dry_run
                    )
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ",
                &[
                    &(entry.slot.as_u64() as i64),
                    &entry.builder_pubkey.as_slice(),
                    &(entry.block_number as i64),
                    &PostgresNumeric::from(entry.delta),
                    &entry.submitted_block_hash.as_slice(),
                    &SystemTime::from(entry.submitted_received_at),
                    &PostgresNumeric::from(entry.submitted_value),
                    &entry.adjusted_block_hash.as_slice(),
                    &PostgresNumeric::from(entry.adjusted_value),
                    &entry.is_dry_run,
                ],
            )
            .await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn get_proposer_header_delivered(
        &self,
        params: &ProposerHeaderDeliveredParams,
    ) -> Result<Vec<ProposerHeaderDeliveredResponse>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_proposer_header_delivered");

        let mut query = String::from(
            "
            SELECT
                slot_number,
                parent_hash,
                block_hash,
                proposer_pubkey,
                value
            FROM get_header
            WHERE mev_boost = true
        ",
        );

        let mut param_index = 1;
        let mut db_params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        if let Some(slot) = params.slot {
            query.push_str(&format!(" AND slot_number = ${param_index}"));
            db_params.push(Box::new(slot as i32));
            param_index += 1;
        }

        if let Some(cursor) = params.cursor {
            query.push_str(&format!(" AND slot_number < ${param_index}"));
            db_params.push(Box::new(cursor as i32));
            param_index += 1;
        }

        if let Some(block_hash) = params.block_hash {
            query.push_str(&format!(" AND block_hash = ${param_index}"));
            db_params.push(Box::new(block_hash.as_slice().to_vec()));
            param_index += 1;
        }

        // builder_pubkey filter not supported (not stored in get_header table)

        if let Some(ref proposer_pubkey) = params.proposer_pubkey {
            query.push_str(&format!(" AND proposer_pubkey = ${param_index}"));
            db_params.push(Box::new(proposer_pubkey.serialize().to_vec()));
            param_index += 1;
        }

        query.push_str(" ORDER BY slot_number DESC");

        if let Some(limit) = params.limit {
            query.push_str(&format!(" LIMIT ${param_index}"));
            db_params.push(Box::new(limit as i64));
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> =
            db_params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        let rows = self.pool.get().await?.query(&query, &params_refs[..]).await?;

        let results = rows
            .iter()
            .map(|row| ProposerHeaderDeliveredResponse {
                slot: row.get::<_, Option<i32>>("slot_number").map(|s| Slot::new(s as u64)),
                parent_hash: row
                    .get::<_, Option<Vec<u8>>>("parent_hash")
                    .and_then(|h| B256::try_from(h.as_slice()).ok()),
                block_hash: row
                    .get::<_, Option<Vec<u8>>>("block_hash")
                    .and_then(|h| B256::try_from(h.as_slice()).ok()),
                proposer_pubkey: row
                    .get::<_, Option<Vec<u8>>>("proposer_pubkey")
                    .and_then(|p| BlsPublicKeyBytes::try_from(p.as_slice()).ok()),
                value: row.get::<_, Option<PostgresNumeric>>("value").map(|v| v.0),
            })
            .collect();

        record.record_success();
        Ok(results)
    }

    #[instrument(skip_all)]
    pub async fn disable_adjustments(&self) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("disable_adjustments");

        self.high_priority_pool
            .get()
            .await?
            .execute("UPDATE relay_info SET adjustments_enabled = FALSE", &[])
            .await?;

        record.record_success();
        Ok(())
    }

    pub async fn check_adjustments_enabled(&self) -> Result<bool, DatabaseError> {
        let mut record = DbMetricRecord::new("check_adjustments_enabled");

        let rows = self
            .pool
            .get()
            .await?
            .query("SELECT adjustments_enabled FROM relay_info LIMIT 1", &[])
            .await?;

        let enabled = rows
            .iter()
            .map(|row| row.get::<_, bool>("adjustments_enabled"))
            .next()
            .unwrap_or(false);

        record.record_success();
        Ok(enabled)
    }

    #[instrument(skip_all)]
    pub async fn save_merged_blocks(
        &self,
        merged_blocks: &[MergedBlock],
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_merged_blocks");

        if merged_blocks.is_empty() {
            record.record_success();
            return Ok(());
        }

        let client = self.pool.get().await?;
        struct MergedBlockParams<'a> {
            slot: i64,
            block_number: i64,
            original_block_hash: &'a [u8],
            block_hash: &'a [u8],
            original_value: PostgresNumeric,
            merged_value: PostgresNumeric,
            original_tx_count: i32,
            merged_tx_count: i32,
            original_blob_count: i32,
            merged_blob_count: i32,
            builder_inclusions: String,
            inserted_at: SystemTime,
        }

        let mut structured_blocks = Vec::with_capacity(merged_blocks.len());
        for block in merged_blocks {
            let builder_inclusions_json = serde_json::to_string(&block.builder_inclusions)
                .map_err(DatabaseError::SerdeJsonError)?;

            structured_blocks.push(MergedBlockParams {
                slot: block.slot as i64,
                block_number: block.block_number as i64,
                original_block_hash: block.original_block_hash.as_slice(),
                block_hash: block.block_hash.as_slice(),
                original_value: PostgresNumeric::from(block.original_value),
                merged_value: PostgresNumeric::from(block.merged_value),
                original_tx_count: block.original_tx_count as i32,
                merged_tx_count: block.merged_tx_count as i32,
                original_blob_count: block.original_blob_count as i32,
                merged_blob_count: block.merged_blob_count as i32,
                builder_inclusions: builder_inclusions_json,
                inserted_at: SystemTime::now(),
            });
        }

        // Flatten into SQL params
        const FIELD_COUNT: usize = 12;
        let mut params: Vec<&(dyn ToSql + Sync)> =
            Vec::with_capacity(structured_blocks.len() * FIELD_COUNT);
        for block in &structured_blocks {
            params.push(&block.slot);
            params.push(&block.block_number);
            params.push(&block.original_block_hash);
            params.push(&block.block_hash);
            params.push(&block.original_value);
            params.push(&block.merged_value);
            params.push(&block.original_tx_count);
            params.push(&block.merged_tx_count);
            params.push(&block.original_blob_count);
            params.push(&block.merged_blob_count);
            params.push(&block.builder_inclusions);
            params.push(&block.inserted_at);
        }

        let mut sql = String::from(
            "INSERT INTO merged_blocks (
                slot,
                block_number,
                original_block_hash,
                block_hash,
                original_value,
                merged_value,
                original_tx_count,
                merged_tx_count,
                original_blob_count,
                merged_blob_count,
                builder_inclusions,
                inserted_at
            ) VALUES ",
        );

        let values_clauses: Vec<String> = (0..structured_blocks.len())
            .map(|i| {
                let start = i * FIELD_COUNT + 1;
                let placeholders: Vec<String> =
                    (0..FIELD_COUNT).map(|j| format!("${}", start + j)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();

        sql.push_str(&values_clauses.join(", "));

        client.execute(&sql, &params).await?;

        record.record_success();
        Ok(())
    }

    pub async fn get_merged_blocks_for_slot(
        &self,
        slot: Slot,
    ) -> Result<Vec<MergedBlockResponse>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_merged_blocks_for_slot");

        let rows = self
            .pool
            .get()
            .await?
            .query(
                "
                SELECT
                    slot,
                    block_number,
                    original_block_hash,
                    block_hash,
                    original_value,
                    merged_value,
                    original_tx_count,
                    merged_tx_count,
                    original_blob_count,
                    merged_blob_count,
                    builder_inclusions,
                    inserted_at
                FROM merged_blocks
                WHERE slot = $1
                ",
                &[&(slot.as_u64() as i64)],
            )
            .await?;
        record.record_success();
        parse_rows(rows)
    }
}
