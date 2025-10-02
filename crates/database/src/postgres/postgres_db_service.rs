use std::{
    collections::HashSet,
    ops::DerefMut,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy_primitives::B256;
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use deadpool_postgres::{Config, GenericClient, ManagerConfig, Pool, RecyclingMethod};
use helix_common::{
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        data_api::BidFilters,
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission, BidSubmission, OptimisticVersion,
    },
    metrics::DbMetricRecord,
    utils::utcnow_ms,
    BuilderInfo, Filtering, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace,
    HeaderSubmissionTrace, PostgresConfig, ProposerInfo, RelayConfig,
    SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorPreferences, ValidatorSummary,
};
use helix_types::{
    BlsPublicKeyBytes, PayloadAndBlobs, SignedBidSubmission, SignedValidatorRegistration,
};
use tokio::sync::mpsc::Sender;
use tokio_postgres::{types::ToSql, NoTls};
use tracing::{error, info, instrument, warn};

use crate::{
    error::DatabaseError,
    postgres::{
        postgres_db_filters::PgBidFilters,
        postgres_db_init::run_migrations_async,
        postgres_db_row_parsing::{parse_bytes_to_pubkey_bytes, parse_row, parse_rows},
        postgres_db_u256_parsing::PostgresNumeric,
    },
    types::{BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument},
    DatabaseService,
};

struct PendingBlockSubmissionValue {
    pub submission: SignedBidSubmission,
    pub trace: SubmissionTrace,
    pub optimistic_version: OptimisticVersion,
}
struct PendingHeaderSubmissionValue {
    pub submission: Arc<SignedHeaderSubmission>,
    pub trace: HeaderSubmissionTrace,
    pub tx_count: u32,
}

const BLOCK_SUBMISSION_FIELD_COUNT: usize = 13;
const HEADER_SUBMISSION_FIELD_COUNT: usize = 13;

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
    validator_registration_cache: Arc<DashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>>,
    pending_validator_registrations: Arc<DashSet<BlsPublicKeyBytes>>,
    block_submissions_sender: Option<Sender<PendingBlockSubmissionValue>>,
    header_submissions_sender: Option<Sender<PendingHeaderSubmissionValue>>,
    known_validators_cache: Arc<DashSet<BlsPublicKeyBytes>>,
    validator_pool_cache: Arc<DashMap<String, String>>,
    region: i16,
    pub pool: Arc<Pool>,
    pub high_priority_pool: Arc<Pool>,
}

impl PostgresDatabaseService {
    pub fn new(cfg: &Config, region: i16) -> Result<Self, Box<dyn std::error::Error>> {
        let pool = cfg.create_pool(None, NoTls)?;
        let high_priority_pool = cfg.create_pool(None, NoTls)?;
        Ok(PostgresDatabaseService {
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            block_submissions_sender: None,
            header_submissions_sender: None,
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region,
            pool: Arc::new(pool),
            high_priority_pool: Arc::new(high_priority_pool),
        })
    }

    pub async fn from_relay_config(relay_config: &RelayConfig) -> Self {
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
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            block_submissions_sender: None,
            header_submissions_sender: None,
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region: relay_config.postgres.region,
            pool: Arc::new(pool),
            high_priority_pool: Arc::new(high_priority_pool),
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

        let client = self.pool.get().await.unwrap();
        let rows = client.query("SELECT * FROM known_validators", &[]).await.unwrap();
        for row in rows {
            let public_key: BlsPublicKeyBytes =
                parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key")).unwrap();
            self.known_validators_cache.insert(public_key);
        }

        record.record_success();
    }

    #[instrument(skip_all)]
    pub async fn load_validator_registrations(&self) {
        let mut record = DbMetricRecord::new("load_validator_registrations");

        match self.get_validator_registrations().await {
            Ok(entries) => {
                let num_entries = entries.len();
                entries.into_iter().for_each(|entry| {
                    self.validator_registration_cache
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

    pub async fn start_processors(&mut self) {
        self.start_registration_processor().await;
        self.start_block_submission_processor().await;
        self.start_header_submission_processor().await;
    }

    pub async fn start_registration_processor(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let self_clone = self.clone();
        tokio::spawn(async move {
            loop {
                interval.tick().await;
                match self_clone.pending_validator_registrations.len() {
                    0 => continue,
                    _ => {
                        let mut entries = Vec::new();
                        for key in self_clone.pending_validator_registrations.iter() {
                            if let Some(entry) = self_clone.validator_registration_cache.get(&*key)
                            {
                                entries.push(entry.clone());
                            }
                        }
                        match self_clone._save_validator_registrations(&entries).await {
                            Ok(_) => {
                                for entry in entries.iter() {
                                    self_clone.pending_validator_registrations.remove(
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

    pub async fn start_block_submission_processor(&mut self) {
        let (block_submissions_sender, mut block_submissions_receiver) =
            tokio::sync::mpsc::channel(10_000);
        self.block_submissions_sender = Some(block_submissions_sender);
        let svc_clone = self.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(1_000);
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            let mut last_slot_processed = 0;
            loop {
                tokio::select! {
                    Some(item) = block_submissions_receiver.recv() => {
                        batch.push(item);
                    }
                    _ = ticker.tick() => {
                        if !batch.is_empty() {
                            if let Err(e) = svc_clone._flush_block_submissions(&batch, &mut last_slot_processed).await {
                                error!("block batch failed: {:?}", e);
                            }
                            batch.clear();
                        }
                    }
                }
            }
        });
    }

    pub async fn start_header_submission_processor(&mut self) {
        let (header_submissions_sender, mut header_submissions_receiver) =
            tokio::sync::mpsc::channel(10_000);
        self.header_submissions_sender = Some(header_submissions_sender);
        let svc_clone = self.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(1_000);
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    Some(item) = header_submissions_receiver.recv() => {
                        batch.push(item);
                    }
                    _ = ticker.tick() => {
                        if !batch.is_empty() {
                            if let Err(e) = svc_clone._flush_header_submissions(&batch).await {
                                error!("header batch failed: {:?}", e);
                            }
                            batch.clear();
                        }
                    }
                }
            }
        });
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
            let mut sql = String::from("INSERT INTO validator_registrations (fee_recipient, gas_limit, timestamp, public_key, signature, inserted_at, user_agent) VALUES ");
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
            let mut sql =
                String::from("INSERT INTO validator_preferences (public_key, filtering, trusted_builders, header_delay, disable_inclusion_lists) VALUES ");
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
        batch: &Vec<PendingBlockSubmissionValue>,
        last_processed_slot: &mut i32,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("flush_block_submissions");
        let client = self.pool.get().await?;

        // Step 1: build structured params for block_submission
        struct BlockParams {
            block_number: i32,
            slot_number: i32,
            parent_hash: Vec<u8>,
            block_hash: Vec<u8>,
            builder_pubkey: Vec<u8>,
            proposer_pubkey: Vec<u8>,
            proposer_fee_recipient: Vec<u8>,
            gas_limit: i32,
            gas_used: i32,
            value: PostgresNumeric,
            num_txs: i32,
            timestamp: i64,
            first_seen: i64,
        }

        let mut structured_blocks: Vec<BlockParams> = Vec::with_capacity(batch.len());
        for item in batch {
            structured_blocks.push(BlockParams {
                block_number: item.submission.block_number() as i32,
                slot_number: item.submission.slot().as_u64() as i32,
                parent_hash: item.submission.parent_hash().as_slice().to_vec(),
                block_hash: item.submission.block_hash().as_slice().to_vec(),
                builder_pubkey: item.submission.builder_public_key().as_slice().to_vec(),
                proposer_pubkey: item.submission.proposer_public_key().as_slice().to_vec(),
                proposer_fee_recipient: item
                    .submission
                    .proposer_fee_recipient()
                    .as_slice()
                    .to_vec(),
                gas_limit: item.submission.gas_limit() as i32,
                gas_used: item.submission.gas_used() as i32,
                value: PostgresNumeric::from(item.submission.value()),
                num_txs: item.submission.num_txs() as i32,
                timestamp: item.submission.timestamp() as i64,
                first_seen: item.trace.receive as i64,
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
        }

        // Build and execute INSERT for block_submission
        let num_cols = BLOCK_SUBMISSION_FIELD_COUNT;
        let mut sql = String::from(
            "INSERT INTO block_submission (block_number, slot_number, parent_hash, block_hash, builder_pubkey, proposer_pubkey, proposer_fee_recipient, gas_limit, gas_used, value, num_txs, timestamp, first_seen) VALUES "
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
        sql.push_str(" ON CONFLICT (block_hash) DO NOTHING");
        client.execute(&sql, &params).await?;

        // Step 2: build structured params for submission_trace
        struct TraceParams {
            block_hash: Vec<u8>,
            region_id: i16,
            optimistic_version: i16,
            receive: i64,
            decode: i64,
            pre_checks: i64,
            signature: i64,
            floor_bid_checks: i64,
            simulation: i64,
            auctioneer_update: i64,
            request_finish: i64,
            metadata: Option<String>,
        }

        let mut structured_traces: Vec<TraceParams> = Vec::with_capacity(batch.len());
        // TODO: refactor tracing
        for item in batch {
            structured_traces.push(TraceParams {
                block_hash: item.submission.block_hash().as_slice().to_vec(),
                region_id: self.region,
                optimistic_version: item.optimistic_version as i16,
                receive: item.trace.receive as i64,
                decode: item.trace.decode as i64,
                pre_checks: 0,
                signature: 0,
                floor_bid_checks: 0,
                simulation: 0,
                auctioneer_update: 0,
                request_finish: 0,
                metadata: item.trace.metadata.clone(),
            });
        }

        let mut ts_params: Vec<&(dyn ToSql + Sync)> =
            Vec::with_capacity(structured_traces.len() * 12);
        for tr in &structured_traces {
            ts_params.push(&tr.block_hash);
            ts_params.push(&tr.region_id);
            ts_params.push(&tr.optimistic_version);
            ts_params.push(&tr.receive);
            ts_params.push(&tr.decode);
            ts_params.push(&tr.pre_checks);
            ts_params.push(&tr.signature);
            ts_params.push(&tr.floor_bid_checks);
            ts_params.push(&tr.simulation);
            ts_params.push(&tr.auctioneer_update);
            ts_params.push(&tr.request_finish);
            ts_params.push(&tr.metadata);
        }

        // Build and execute INSERT for submission_trace
        let trace_cols = 12;
        let mut ts_sql = String::from(
            "INSERT INTO submission_trace (block_hash, region_id, optimistic_version, receive, decode, pre_checks, signature, floor_bid_checks, simulation, auctioneer_update, request_finish, metadata) VALUES "
        );
        let ts_clauses: Vec<String> = (0..structured_traces.len())
            .map(|i| {
                let start = i * trace_cols + 1;
                let placeholders: Vec<String> =
                    (0..trace_cols).map(|j| format!("${}", start + j)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();
        ts_sql.push_str(&ts_clauses.join(", "));
        client.execute(&ts_sql, &ts_params).await?;

        // Step 3: build structured params for slot_preferences, skipping already processed slots
        // Collect only new slots
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
                "INSERT INTO slot_preferences (slot_number, proposer_pubkey, filtering, trusted_builders, header_delay) "
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

    /// Flush header submissions in bulk using structured params
    async fn _flush_header_submissions(
        &self,
        batch: &Vec<PendingHeaderSubmissionValue>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("flush_header_submissions");
        let client = self.pool.get().await?;

        // Step 1: structured params for header_submission
        struct HeaderParams {
            block_number: i32,
            slot_number: i32,
            parent_hash: Vec<u8>,
            block_hash: Vec<u8>,
            builder_pubkey: Vec<u8>,
            proposer_pubkey: Vec<u8>,
            proposer_fee_recipient: Vec<u8>,
            gas_limit: i32,
            gas_used: i32,
            value: PostgresNumeric,
            timestamp: i64,
            first_seen: i64,
            tx_count: i32,
        }

        let mut structured_headers: Vec<HeaderParams> = Vec::with_capacity(batch.len());
        for item in batch {
            let hdr = item.submission.execution_payload_header();
            structured_headers.push(HeaderParams {
                block_number: hdr.block_number as i32,
                slot_number: item.submission.slot().as_u64() as i32,
                parent_hash: item.submission.parent_hash().as_slice().to_vec(),
                block_hash: item.submission.block_hash().as_slice().to_vec(),
                builder_pubkey: item.submission.builder_public_key().as_slice().to_vec(),
                proposer_pubkey: item.submission.proposer_public_key().as_slice().to_vec(),
                proposer_fee_recipient: item
                    .submission
                    .proposer_fee_recipient()
                    .as_slice()
                    .to_vec(),
                gas_limit: item.submission.gas_limit() as i32,
                gas_used: item.submission.gas_used() as i32,
                value: PostgresNumeric::from(item.submission.value()),
                timestamp: item.submission.timestamp() as i64,
                first_seen: item.trace.receive as i64,
                tx_count: item.tx_count as i32,
            });
        }

        let mut params: Vec<&(dyn ToSql + Sync)> =
            Vec::with_capacity(structured_headers.len() * HEADER_SUBMISSION_FIELD_COUNT);
        for hdr in &structured_headers {
            params.push(&hdr.block_number);
            params.push(&hdr.slot_number);
            params.push(&hdr.parent_hash);
            params.push(&hdr.block_hash);
            params.push(&hdr.builder_pubkey);
            params.push(&hdr.proposer_pubkey);
            params.push(&hdr.proposer_fee_recipient);
            params.push(&hdr.gas_limit);
            params.push(&hdr.gas_used);
            params.push(&hdr.value);
            params.push(&hdr.timestamp);
            params.push(&hdr.first_seen);
            params.push(&hdr.tx_count);
        }

        // Build and execute INSERT for header_submission
        let cols = HEADER_SUBMISSION_FIELD_COUNT;
        let mut sql = String::from(
            "INSERT INTO header_submission (block_number, slot_number, parent_hash, block_hash, builder_pubkey, proposer_pubkey, proposer_fee_recipient, gas_limit, gas_used, value, timestamp, first_seen, tx_count) VALUES "
        );
        let clauses: Vec<String> = (0..structured_headers.len())
            .map(|i| {
                let start = i * cols + 1;
                let placeholders: Vec<String> =
                    (0..cols).map(|j| format!("${}", start + j)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();
        sql.push_str(&clauses.join(", "));
        sql.push_str(" ON CONFLICT (block_hash) DO NOTHING");
        client.execute(&sql, &params).await?;

        // Step 2: structured params for header_submission_trace
        struct HTraceParams {
            block_hash: Vec<u8>,
            region_id: i16,
            receive: i64,
            decode: i64,
            pre_checks: i64,
            signature: i64,
            floor_bid_checks: i64,
            auctioneer_update: i64,
            request_finish: i64,
            metadata: Option<String>,
        }

        let mut structured_htraces: Vec<HTraceParams> = Vec::with_capacity(batch.len());
        for item in batch {
            structured_htraces.push(HTraceParams {
                block_hash: item.submission.block_hash().as_slice().to_vec(),
                region_id: self.region,
                receive: item.trace.receive as i64,
                decode: item.trace.decode as i64,
                pre_checks: item.trace.pre_checks as i64,
                signature: item.trace.signature as i64,
                floor_bid_checks: item.trace.signature as i64,
                auctioneer_update: item.trace.auctioneer_update as i64,
                request_finish: item.trace.request_finish as i64,
                metadata: item.trace.metadata.clone(),
            });
        }

        let mut hts_params: Vec<&(dyn ToSql + Sync)> =
            Vec::with_capacity(structured_htraces.len() * 10);
        for ht in &structured_htraces {
            hts_params.push(&ht.block_hash);
            hts_params.push(&ht.region_id);
            hts_params.push(&ht.receive);
            hts_params.push(&ht.decode);
            hts_params.push(&ht.pre_checks);
            hts_params.push(&ht.signature);
            hts_params.push(&ht.floor_bid_checks);
            hts_params.push(&ht.auctioneer_update);
            hts_params.push(&ht.request_finish);
            hts_params.push(&ht.metadata);
        }

        // Build and execute INSERT for header_submission_trace
        let trace_cols = 10;
        let mut tsql = String::from(
            "INSERT INTO header_submission_trace (block_hash, region_id, receive, decode, pre_checks, signature, floor_bid_checks, auctioneer_update, request_finish, metadata) VALUES "
        );
        let tclauses: Vec<String> = (0..structured_htraces.len())
            .map(|i| {
                let start = i * trace_cols + 1;
                let placeholders: Vec<String> =
                    (0..trace_cols).map(|j| format!("${}", start + j)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();
        tsql.push_str(&tclauses.join(", "));
        client.execute(&tsql, &hts_params).await?;

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
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            block_submissions_sender: None,
            header_submissions_sender: None,
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region: 1,
            pool: Arc::new(pool),
            high_priority_pool: Arc::new(high_priority_pool),
        }
    }
}

#[async_trait]
impl DatabaseService for PostgresDatabaseService {
    #[instrument(skip_all)]
    async fn save_validator_registrations(
        &self,
        mut entries: Vec<ValidatorRegistrationInfo>,
        pool_name: Option<String>,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_validator_registrations");

        entries.retain(|entry| {
            if let Some(existing_entry) =
                self.validator_registration_cache.get(&entry.registration.message.pubkey)
            {
                if existing_entry.registration_info.registration.message.timestamp >=
                    entry.registration.message.timestamp
                {
                    return false;
                }
            }
            true
        });

        for entry in entries.iter() {
            self.pending_validator_registrations.insert(entry.registration.message.pubkey);
            self.validator_registration_cache.insert(
                entry.registration.message.pubkey,
                SignedValidatorRegistrationEntry::new(
                    entry.clone(),
                    pool_name.clone(),
                    user_agent.clone(),
                ),
            );
        }

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn is_registration_update_required(
        &self,
        registration: &SignedValidatorRegistration,
    ) -> Result<bool, DatabaseError> {
        if let Some(existing_entry) =
            self.validator_registration_cache.get(&registration.message.pubkey)
        {
            if existing_entry.registration_info.registration.message.timestamp >=
                registration.message.timestamp
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[instrument(skip_all)]
    async fn get_validator_registration(
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
    async fn get_validator_registrations_for_pub_keys(
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
    async fn set_proposer_duties(
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

    #[instrument(skip_all)]
    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_proposer_duties");

        let rows = self
            .high_priority_pool
            .get()
            .await?
            .query(
                "
                            SELECT * FROM proposer_duties
                            INNER JOIN validator_registrations
                            ON proposer_duties.public_key = validator_registrations.public_key
                            INNER JOIN validator_preferences
                            ON proposer_duties.public_key = validator_preferences.public_key
                            WHERE validator_registrations.active = true
                        ",
                &[],
            )
            .await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    async fn get_validator_preferences(
        &self,
        pub_key: &BlsPublicKeyBytes,
    ) -> Result<Option<ValidatorPreferences>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_validator_preferences");

        if let Some(cached_entry) = self.validator_registration_cache.get(pub_key) {
            record.record_success();
            return Ok(Some(cached_entry.registration_info.preferences.clone()));
        }

        let rows = self
            .pool
            .get()
            .await?
            .query(
                "
                SELECT
                    validator_preferences.filtering,
                    validator_preferences.trusted_builders,
                    validator_preferences.header_delay,
                    validator_preferences.delay_ms,
                    validator_preferences.disable_inclusion_lists
                FROM validator_preferences
                WHERE validator_preferences.public_key = $1
                ",
                &[&(pub_key.as_slice())],
            )
            .await?;

        let result = if rows.is_empty() {
            None
        } else {
            let prefs: ValidatorPreferences = parse_row(&rows[0])?;
            Some(prefs)
        };

        record.record_success();
        Ok(result)
    }

    async fn update_validator_preferences(
        &self,
        pub_key: &BlsPublicKeyBytes,
        preferences: &ValidatorPreferences,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("update_validator_preferences");

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        let trusted_builders: Option<Vec<String>> =
            preferences.trusted_builders.as_ref().map(|builders| builders.to_vec());

        let filtering_value: i16 = match preferences.filtering {
            helix_common::Filtering::Regional => 1,
            helix_common::Filtering::Global => 0,
        };

        transaction
            .execute(
                "
                INSERT INTO validator_preferences (
                    public_key,
                    filtering,
                    trusted_builders,
                    header_delay,
                    delay_ms,
                    disable_inclusion_lists,
                    manual_override
                ) VALUES ($1, $2, $3, $4, $5, $6, TRUE)
                ON CONFLICT (public_key) 
                DO UPDATE SET
                    filtering = EXCLUDED.filtering,
                    trusted_builders = EXCLUDED.trusted_builders,
                    header_delay = EXCLUDED.header_delay,
                    delay_ms = EXCLUDED.delay_ms,
                    disable_inclusion_lists = EXCLUDED.disable_inclusion_lists,
                    manual_override = TRUE
                ",
                &[
                    &pub_key.as_slice(),
                    &filtering_value,
                    &trusted_builders,
                    &preferences.header_delay,
                    &preferences.delay_ms.map(|v| v as i64),
                    &preferences.disable_inclusion_lists,
                ],
            )
            .await?;

        transaction.commit().await?;

        if let Some(mut cached_entry) = self.validator_registration_cache.get_mut(pub_key) {
            cached_entry.registration_info.preferences = preferences.clone();
        }

        record.record_success();

        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_pool_validators(
        &self,
        pool_name: &str,
    ) -> Result<Vec<BlsPublicKeyBytes>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_pool_validators");

        let rows = self
            .pool
            .get()
            .await?
            .query(
                "
                SELECT public_key
                FROM trusted_proposers
                WHERE name = $1
                ",
                &[&pool_name],
            )
            .await?;

        let mut validators = Vec::new();
        for row in rows {
            let pubkey_bytes: Vec<u8> = row.try_get("public_key")?;

            if pubkey_bytes.len() == 48 {
                let mut key = [0u8; 48];
                key.copy_from_slice(&pubkey_bytes);
                validators.push(BlsPublicKeyBytes::from(key));
            } else {
                error!(
                    length = pubkey_bytes.len(),
                    "Invalid validator pubkey length in trusted_proposers"
                );
            }
        }

        record.record_success();
        Ok(validators)
    }

    #[instrument(skip_all)]
    async fn set_known_validators(
        &self,
        known_validators: Vec<ValidatorSummary>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("set_known_validators");

        info!("Known validators: current cache size: {:?}", self.known_validators_cache.len());

        let mut client = self.pool.get().await?;

        let new_keys_set: HashSet<BlsPublicKeyBytes> =
            known_validators.iter().map(|validator| validator.validator.pubkey).collect();

        let old_keys_hash_set: HashSet<BlsPublicKeyBytes> = self
            .known_validators_cache
            .iter()
            .map(|ref_multi| *ref_multi.key()) // Access and clone the key from RefMulti
            .collect();

        let keys_to_add: Vec<BlsPublicKeyBytes> =
            new_keys_set.difference(&old_keys_hash_set).cloned().collect();
        let keys_to_remove: Vec<BlsPublicKeyBytes> =
            old_keys_hash_set.difference(&new_keys_set).cloned().collect();

        for key in &keys_to_add {
            self.known_validators_cache.insert(*key);
        }
        for key in &keys_to_remove {
            self.known_validators_cache.remove(key);
        }

        info!("Known validators: added: {:?}", keys_to_add.len());
        info!("Known validators: removed: {:?}", keys_to_add.len());

        info!("Known validators: updated cache size: {:?}", self.known_validators_cache.len());

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
    async fn check_known_validators(
        &self,
        public_keys: Vec<BlsPublicKeyBytes>,
    ) -> Result<HashSet<BlsPublicKeyBytes>, DatabaseError> {
        let mut record = DbMetricRecord::new("check_known_validators");

        let client = self.high_priority_pool.get().await?;
        let mut pub_keys = HashSet::new();

        for public_key in public_keys.iter() {
            if self.known_validators_cache.contains(public_key) {
                pub_keys.insert(*public_key);
            } else {
                let rows = client
                    .query("SELECT * FROM known_validators WHERE public_key = $1", &[
                        &(public_key.to_vec())
                    ])
                    .await?;
                for row in rows {
                    let public_key: BlsPublicKeyBytes =
                        parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key"))?;
                    self.known_validators_cache.insert(public_key);
                    pub_keys.insert(public_key);
                }
            }
        }

        record.record_success();
        Ok(pub_keys)
    }

    #[instrument(skip_all)]
    async fn get_validator_pool_name(
        &self,
        api_key: &str,
    ) -> Result<Option<String>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_validator_pool_name");

        let client = self.high_priority_pool.get().await?;

        if self.validator_pool_cache.is_empty() {
            let rows = client.query("SELECT * FROM validator_pools", &[]).await?;
            for row in rows {
                let api_key: String = row.get::<&str, &str>("api_key").to_string();
                let name: String = row.get::<&str, &str>("name").to_string();
                self.validator_pool_cache.insert(api_key, name);
            }
        }

        if self.validator_pool_cache.contains_key(api_key) {
            return Ok(self.validator_pool_cache.get(api_key).map(|f| f.clone()));
        }

        let api_key = api_key.to_string();
        let rows = match client
            .query("SELECT * FROM validator_pools WHERE api_key = $1", &[&api_key])
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!("Error querying validator_pools: {}", e);
                return Err(DatabaseError::from(e));
            }
        };

        if rows.is_empty() {
            return Ok(None);
        }

        let name: String = rows[0].get("name");

        self.validator_pool_cache.insert(api_key.to_string(), name.clone());

        record.record_success();
        Ok(Some(name))
    }

    #[instrument(skip_all)]
    async fn save_too_late_get_payload(
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
    async fn save_delivered_payload(
        &self,
        proposer_pub_key: BlsPublicKeyBytes,
        payload: Arc<PayloadAndBlobs>,
        latency_trace: &GetPayloadTrace,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        latency_trace.record_metrics();
        let mut record = DbMetricRecord::new("save_delivered_payload");

        let region_id = self.region;
        let block_hash = payload.execution_payload.block_hash;
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        transaction.execute(
            "
                INSERT INTO delivered_payload 
                    (block_hash, payload_parent_hash, fee_recipient, state_root, receipts_root, logs_bloom, prev_randao, timestamp, block_number, gas_limit, gas_used, extra_data, base_fee_per_gas, user_agent)
                VALUES 
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                ON CONFLICT (block_hash)
                DO NOTHING
            ",
            &[
                &(block_hash.as_slice()),
                &(payload.execution_payload.parent_hash.as_slice()),
                &(payload.execution_payload.fee_recipient.as_slice()),
                &(payload.execution_payload.state_root.as_slice()),
                &(payload.execution_payload.receipts_root.as_slice()),
                &(payload.execution_payload.logs_bloom.to_vec()),
                &(payload.execution_payload.prev_randao.as_slice()),
                &(payload.execution_payload.timestamp as i64),
                &(payload.execution_payload.block_number as i32),
                &(payload.execution_payload.gas_limit as i32),
                &(payload.execution_payload.gas_used as i32),
                &(payload.execution_payload.extra_data.to_vec()),
                &(PostgresNumeric::from(payload.execution_payload.base_fee_per_gas)),
                &(user_agent),
            ],
            ).await?;

        transaction.execute(
                "
                    INSERT INTO delivered_payload_preferences (block_hash, filtering, trusted_builders)
                    SELECT $1::bytea, filtering, trusted_builders
                    FROM validator_preferences
                    WHERE public_key = $2::bytea
                    ON CONFLICT (block_hash) DO NOTHING;                
                ",
                &[
                    &(block_hash.as_slice()),
                    &(proposer_pub_key.as_slice()),
                ],
                ).await?;

        transaction.execute(
            "
                INSERT INTO payload_trace
                    (block_hash, region_id, receive, proposer_index_validated, signature_validated, payload_fetched, validation_complete, beacon_client_broadcast, broadcaster_block_broadcast, on_deliver_payload)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ",
            &[
                &(block_hash.as_slice()),
                &(region_id),
                &(latency_trace.receive as i64),
                &(latency_trace.proposer_index_validated as i64),
                &(latency_trace.signature_validated as i64),
                &(latency_trace.payload_fetched as i64),
                &(latency_trace.validation_complete as i64),
                &(latency_trace.beacon_client_broadcast as i64),
                &(latency_trace.broadcaster_block_broadcast as i64),
                &(latency_trace.on_deliver_payload as i64),
            ],
        ).await?;

        if !payload.execution_payload.transactions.is_empty() {
            // Save the transactions
            let mut structured_params: Vec<(Vec<u8>, &[u8])> = Vec::new();
            for entry in payload.execution_payload.transactions.iter() {
                structured_params
                    .push((payload.execution_payload.block_hash.to_vec(), entry.as_ref()));
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

            transaction.execute(&sql, &params[..]).await?;
        }

        if !payload.execution_payload.withdrawals.is_empty() {
            // Save the withdrawals
            let mut structured_params: Vec<(i32, Vec<u8>, i32, &[u8], i64)> = Vec::new();
            for entry in payload.execution_payload.withdrawals.iter() {
                structured_params.push((
                    entry.index as i32,
                    payload.execution_payload.block_hash.to_vec(),
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

            transaction.execute(&sql, &params[..]).await?;
        }

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn store_block_submission(
        &self,
        submission: SignedBidSubmission,
        trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
    ) -> Result<(), DatabaseError> {
        trace.record_metrics(optimistic_version);
        let mut record = DbMetricRecord::new("store_block_submission");
        if let Some(sender) = &self.block_submissions_sender {
            sender
                .send(PendingBlockSubmissionValue { submission, trace, optimistic_version })
                .await
                .map_err(|_| DatabaseError::ChannelSendError)?;
        } else {
            return Err(DatabaseError::ChannelSendError);
        }
        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn store_builder_info(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
        builder_info: &BuilderInfo,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("store_builder_info");

        info!("Storing builder info for {:?}", builder_pub_key);

        self.pool
            .get()
            .await?
            .execute(
                "
                    INSERT INTO builder_info (public_key, collateral, is_optimistic, is_optimistic_for_regional_filtering, builder_id, builder_ids)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    ON CONFLICT (public_key)
                    DO UPDATE SET
                        builder_ids = array_concat_uniq(COALESCE(builder_info.builder_ids, '{}'::character varying[]), EXCLUDED.builder_ids)
                ",
                &[
                    &(builder_pub_key.as_slice()),
                    &(PostgresNumeric::from(builder_info.collateral)),
                    &(builder_info.is_optimistic),
                    &(builder_info.is_optimistic_for_regional_filtering),
                    &(builder_info.builder_id),
                    &(builder_info.builder_ids),
                ],
            )
            .await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn store_builders_info(
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
    async fn check_builder_api_key(&self, api_key: &str) -> Result<bool, DatabaseError> {
        let mut record = DbMetricRecord::new("check_builder_api_key");

        let client = self.high_priority_pool.get().await?;
        let rows =
            client.query("SELECT * FROM builder_info WHERE api_key = $1", &[&(api_key)]).await?;

        record.record_success();
        Ok(!rows.is_empty())
    }

    #[instrument(skip_all)]
    async fn db_demote_builder(
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
    async fn get_bids(
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
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        let query = format!("{block_query} UNION {header_query}");

        let rows = self.pool.get().await?.query(&query, &params_refs[..]).await?;

        record.record_success();
        parse_rows(rows)
    }

    #[instrument(skip_all)]
    async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let mut record = DbMetricRecord::new("get_delivered_payloads");

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

        if validator_preferences.trusted_builders.is_some() || filtering.is_some() {
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

        if let Some(trusted_builders) = &validator_preferences.trusted_builders {
            query.push_str(&format!(
                " AND delivered_payload_preferences.trusted_builders @> ${param_index}"
            ));
            params.push(Box::new(trusted_builders));
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
    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: B256,
        public_key: BlsPublicKeyBytes,
        best_block_hash: B256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("save_get_header_call");

        let region_id = self.region;

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                    INSERT INTO get_header
                        (slot_number, region_id, parent_hash, proposer_pubkey, block_hash, mev_boost, user_agent)
                    VALUES
                        ($1, $2, $3, $4, $5, $6, $7)
                ",
                &[
                    &(slot as i32),
                    &(region_id),
                    &(parent_hash.as_slice()),
                    &(public_key.as_slice()),
                    &(best_block_hash.as_slice()),
                    &(mev_boost),
                    &(user_agent),
                ],
            )
            .await?;

        transaction
            .execute(
                "
                INSERT INTO get_header_trace
                    (block_hash, region_id, receive, validation_complete, best_bid_fetched)
                VALUES
                    ($1, $2, $3, $4, $5)
            ",
                &[
                    &(best_block_hash.as_slice()),
                    &(region_id),
                    &(trace.receive as i64),
                    &(trace.validation_complete as i64),
                    &(trace.best_bid_fetched as i64),
                ],
            )
            .await?;

        transaction.commit().await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn save_failed_get_payload(
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
    async fn store_header_submission(
        &self,
        submission: Arc<SignedHeaderSubmission>,
        trace: HeaderSubmissionTrace,
        tx_count: u32,
    ) -> Result<(), DatabaseError> {
        let mut record = DbMetricRecord::new("store_header_submission");
        if let Some(sender) = &self.header_submissions_sender {
            sender
                .send(PendingHeaderSubmissionValue {
                    submission: submission.clone(),
                    trace,
                    tx_count,
                })
                .await
                .map_err(|_| DatabaseError::ChannelSendError)?;
        } else {
            return Err(DatabaseError::ChannelSendError);
        }
        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn save_gossiped_payload_trace(
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
    async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError> {
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
    async fn save_inclusion_list(
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
}
