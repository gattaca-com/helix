#![allow(unused)]
use std::{
    collections::HashSet,
    ops::DerefMut,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use deadpool_postgres::{Config, GenericClient, ManagerConfig, Pool, RecyclingMethod};
use ethereum_consensus::{altair::Hash32, primitives::BlsPublicKey, ssz::prelude::ByteVector};
use helix_common::{
    api::{builder_api::BuilderGetValidatorsResponseEntry, data_api::BidFilters, proposer_api::ValidatorRegistrationInfo},
    bid_submission::{v2::header_submission::SignedHeaderSubmission, BidSubmission, BidTrace, SignedBidSubmission},
    deneb::SignedValidatorRegistration,
    simulator::BlockSimError,
    versioned_payload::PayloadAndBlobs,
    BuilderInfo, Filtering, GetHeaderTrace, GetPayloadTrace, GossipedHeaderTrace, GossipedPayloadTrace, HeaderSubmissionTrace, ProposerInfo,
    RelayConfig, SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorPreferences, ValidatorSummary,
};
use tokio_postgres::{types::ToSql, NoTls};
use tracing::{error, info};

use crate::{
    error::DatabaseError,
    postgres::{
        postgres_db_filters::PgBidFilters,
        postgres_db_init::run_migrations_async,
        postgres_db_row_parsing::{parse_bytes_to_pubkey, parse_row, parse_rows},
        postgres_db_u256_parsing::PostgresNumeric,
    },
    types::{BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument},
    DatabaseService,
};

struct RegistrationParams<'a> {
    fee_recipient: &'a [u8],
    gas_limit: i32,
    timestamp: i64,
    public_key: &'a [u8],
    signature: &'a [u8],
    inserted_at: SystemTime,
}

struct PreferenceParams<'a> {
    public_key: &'a [u8],
    filtering: i16,
    trusted_builders: Option<Vec<String>>,
    header_delay: bool,
}

struct TrustedProposerParams<'a> {
    public_key: &'a [u8],
    name: Option<String>,
}

#[derive(Clone)]
pub struct PostgresDatabaseService {
    validator_registration_cache: Arc<DashMap<BlsPublicKey, SignedValidatorRegistrationEntry>>,
    pending_validator_registrations: Arc<DashSet<BlsPublicKey>>,
    known_validators_cache: Arc<DashSet<BlsPublicKey>>,
    validator_pool_cache: Arc<DashMap<String, String>>,
    region: i16,
    pool: Arc<Pool>,
}

impl PostgresDatabaseService {
    pub fn new(cfg: &Config, region: i16) -> Result<Self, Box<dyn std::error::Error>> {
        let pool = cfg.create_pool(None, NoTls)?;
        Ok(PostgresDatabaseService {
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region,
            pool: Arc::new(pool),
        })
    }

    pub fn from_relay_config(relay_config: &RelayConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let mut cfg = Config::new();
        cfg.host = Some(relay_config.postgres.hostname.clone());
        cfg.port = Some(relay_config.postgres.port.clone());
        cfg.dbname = Some(relay_config.postgres.db_name.clone());
        cfg.user = Some(relay_config.postgres.user.clone());
        cfg.password = Some(relay_config.postgres.password.clone());
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = cfg.create_pool(None, NoTls)?;
        Ok(PostgresDatabaseService {
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region: relay_config.postgres.region,
            pool: Arc::new(pool),
        })
    }

    pub async fn run_migrations(&self) {
        let mut conn = self.pool.get().await.unwrap();
        let client = conn.deref_mut().deref_mut();
        match run_migrations_async(client).await {
            Ok(report) => {
                info!("Applied migrations: {}", report.applied_migrations().len());
                info!("Migrations report: {:?}", report);
            }
            Err(e) => {
                panic!("Error applying migrations: {}", e);
            }
        };
    }

    pub async fn init_region(&self, config: &RelayConfig) {
        let client = self.pool.get().await.unwrap();
        match client
            .execute(
                "
                INSERT INTO region (id, name)
                VALUES ($1, $2)
                ON CONFLICT (id)
                DO NOTHING
            ",
                &[&(config.postgres.region), &(config.postgres.region_name)],
            )
            .await
        {
            Ok(_) => {
                info!("Region {} initialized", config.postgres.region);
            }
            Err(e) => {
                panic!("Error initializing region {}: {}", config.postgres.region, e);
            }
        };
    }

    pub async fn load_known_validators(&self) {
        let client = self.pool.get().await.unwrap();
        let rows = client.query("SELECT * FROM known_validators", &[]).await.unwrap();
        for row in rows {
            let public_key: BlsPublicKey = parse_bytes_to_pubkey(row.get::<&str, &[u8]>("public_key")).unwrap();
            self.known_validators_cache.insert(public_key);
        }
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
                            if let Some(entry) = self_clone.validator_registration_cache.get(&*key) {
                                entries.push(entry.clone());
                            }
                        }
                        match self_clone._save_validator_registrations(&entries).await {
                            Ok(_) => {
                                for entry in entries.iter() {
                                    self_clone.pending_validator_registrations.remove(&entry.registration_info.registration.message.public_key);
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

    async fn _save_validator_registrations(&self, entries: &[SignedValidatorRegistrationEntry]) -> Result<(), DatabaseError> {
        let mut client = self.pool.get().await?;

        let batch_size = 1000;
        for chunk in entries.chunks(batch_size) {
            let transaction = client.transaction().await?;

            let mut structured_params_for_reg: Vec<RegistrationParams> = Vec::with_capacity(chunk.len());

            let mut structured_params_for_pref: Vec<PreferenceParams> = Vec::with_capacity(chunk.len());

            let mut structured_params_for_trusted: Vec<TrustedProposerParams> = Vec::with_capacity(chunk.len());

            for entry in chunk.iter() {
                let registration = &entry.registration_info.registration.message;
                let fee_recipient = &registration.fee_recipient;
                let public_key = &registration.public_key;
                let signature = &entry.registration_info.registration.signature;
                let name = &entry.pool_name;

                let inserted_at = SystemTime::now();

                // Collect the parameters in a structured manner
                structured_params_for_reg.push(RegistrationParams {
                    fee_recipient: fee_recipient.as_ref(),
                    gas_limit: registration.gas_limit as i32,
                    timestamp: registration.timestamp as i64,
                    public_key: public_key.as_ref(),
                    signature: signature.as_ref(),
                    inserted_at,
                });

                structured_params_for_pref.push(PreferenceParams {
                    public_key: public_key.as_ref(),
                    filtering: entry.registration_info.preferences.filtering.clone() as i16,
                    trusted_builders: entry.registration_info.preferences.trusted_builders.clone(),
                    header_delay: entry.registration_info.preferences.header_delay,
                });

                if name.is_some() {
                    structured_params_for_trusted.push(TrustedProposerParams { public_key: public_key.as_ref(), name: name.clone() });
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
                    ]
                })
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql =
                String::from("INSERT INTO validator_registrations (fee_recipient, gas_limit, timestamp, public_key, signature, inserted_at) VALUES ");
            let num_params_per_row = 6;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (public_key) DO UPDATE SET fee_recipient = excluded.fee_recipient, gas_limit = excluded.gas_limit, timestamp = excluded.timestamp, signature = excluded.signature, inserted_at = excluded.inserted_at");

            // Execute the query
            transaction.execute(&sql, &params[..]).await?;

            let params: Vec<&(dyn ToSql + Sync)> = structured_params_for_pref
                .iter()
                .flat_map(|tuple| vec![&tuple.public_key as &(dyn ToSql + Sync), &tuple.filtering, &tuple.trusted_builders, &tuple.header_delay])
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO validator_preferences (public_key, filtering, trusted_builders, header_delay) VALUES ");
            let num_params_per_row = 4;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (public_key) DO UPDATE SET filtering = excluded.filtering, trusted_builders = excluded.trusted_builders, header_delay = excluded.header_delay");

            // Execute the query
            transaction.execute(&sql, &params[..]).await?;

            if structured_params_for_trusted.is_empty() {
                transaction.commit().await?;
                continue;
            }

            let params: Vec<&(dyn ToSql + Sync)> =
                structured_params_for_trusted.iter().flat_map(|tuple| vec![&tuple.public_key as &(dyn ToSql + Sync), &tuple.name]).collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO trusted_proposers (pub_key, name) VALUES ");
            let num_params_per_row = 2;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
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

        PostgresDatabaseService {
            validator_registration_cache: Arc::new(DashMap::new()),
            pending_validator_registrations: Arc::new(DashSet::new()),
            known_validators_cache: Arc::new(DashSet::new()),
            validator_pool_cache: Arc::new(DashMap::new()),
            region: 1,
            pool: Arc::new(pool),
        }
    }
}

#[async_trait]
impl DatabaseService for PostgresDatabaseService {
    async fn save_validator_registration(
        &self,
        registration_info: ValidatorRegistrationInfo,
        pool_name: Option<String>,
    ) -> Result<(), DatabaseError> {
        let registration = registration_info.registration.message.clone();

        if let Some(entry) = self.validator_registration_cache.get(&registration.public_key) {
            if entry.registration_info.registration.message.timestamp >= registration.timestamp {
                return Ok(());
            }
        }

        let fee_recipient = &registration.fee_recipient;
        let public_key = &registration.public_key;
        let signature = &registration_info.registration.signature;

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        let inserted_at = SystemTime::now();

        transaction
            .execute(
                "INSERT INTO validator_preferences (public_key, filtering, trusted_builders, header_delay)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (public_key)
            DO UPDATE SET
                filtering = excluded.filtering, trusted_builders = excluded.trusted_builders, header_delay = excluded.header_delay
            ",
                &[
                    &public_key.as_ref(),
                    &(registration_info.preferences.filtering as i16),
                    &registration_info.preferences.trusted_builders,
                    &registration_info.preferences.header_delay,
                ],
            )
            .await?;

        match transaction
            .execute(
                "
                INSERT INTO validator_registrations (fee_recipient, gas_limit, timestamp, public_key, signature, inserted_at)
                VALUES ($1, $2, $3, $4, $5,$6)
                ON CONFLICT (public_key)
                DO UPDATE SET
                    fee_recipient = excluded.fee_recipient,
                    gas_limit = excluded.gas_limit,
                    timestamp = excluded.timestamp,
                    signature = excluded.signature,
                    inserted_at = excluded.inserted_at
            ",
                &[
                    &(fee_recipient.as_ref()),
                    &(registration.gas_limit as i32),
                    &(registration.timestamp as i64),
                    &(public_key.as_ref()),
                    &(signature.as_ref()),
                    &(inserted_at),
                ],
            )
            .await
        {
            Ok(_) => {
                self.validator_registration_cache.insert(public_key.clone(), SignedValidatorRegistrationEntry {
                    registration_info,
                    inserted_at: inserted_at.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                    pool_name,
                });
            }
            Err(e) => return Err(DatabaseError::from(e)),
        };

        transaction.commit().await?;

        Ok(())
    }

    async fn save_validator_registrations(
        &self,
        mut entries: Vec<ValidatorRegistrationInfo>,
        pool_name: Option<String>,
    ) -> Result<(), DatabaseError> {
        entries.retain(|entry| {
            if let Some(existing_entry) = self.validator_registration_cache.get(&entry.registration.message.public_key) {
                if existing_entry.registration_info.registration.message.timestamp >= entry.registration.message.timestamp {
                    return false;
                }
            }
            true
        });

        for entry in entries.iter() {
            self.pending_validator_registrations.insert(entry.registration.message.public_key.clone());
            self.validator_registration_cache
                .insert(entry.registration.message.public_key.clone(), SignedValidatorRegistrationEntry::new(entry.clone(), pool_name.clone()));
        }

        Ok(())
    }

    async fn is_registration_update_required(&self, registration: &SignedValidatorRegistration) -> Result<bool, DatabaseError> {
        if let Some(existing_entry) = self.validator_registration_cache.get(&registration.message.public_key) {
            if existing_entry.registration_info.registration.message.timestamp >= registration.message.timestamp {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn get_validator_registration(&self, pub_key: BlsPublicKey) -> Result<SignedValidatorRegistrationEntry, DatabaseError> {
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
                    validator_registrations.inserted_at
                FROM validator_registrations
                INNER JOIN validator_preferences ON validator_registrations.public_key = validator_preferences.public_key
                WHERE validator_registrations.public_key = $1
            ",
                &[&(pub_key.as_ref())],
            )
            .await?
        {
            rows if rows.is_empty() => Err(DatabaseError::ValidatorRegistrationNotFound),
            rows => parse_row(rows.first().unwrap()),
        }
    }

    async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: Vec<BlsPublicKey>,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let client = self.pool.get().await.map_err(DatabaseError::from)?;

        // Constructing the query
        let placeholders: Vec<String> = (1..=pub_keys.len()).map(|i| format!("${}", i)).collect();
        let query = format!(
            "SELECT *
            FROM validator_registrations
            INNER JOIN validator_preferences ON validator_registrations.public_key = validator_preferences.public_key
            WHERE validator_preferences.public_key IN ({})",
            placeholders.join(", ")
        );

        // Preparing the query
        let stmt = client.prepare(&query).await.map_err(DatabaseError::from)?;

        let params: Vec<Box<dyn ToSql + Sync + Send>> =
            pub_keys.iter().map(|key: &BlsPublicKey| Box::new(key.as_ref()) as Box<dyn ToSql + Sync + Send>).collect();

        let params_slice: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();

        parse_rows(client.query(&stmt, &params_slice).await.map_err(DatabaseError::from)?)
    }

    async fn get_validator_registration_timestamp(&self, pub_key: BlsPublicKey) -> Result<u64, DatabaseError> {
        self.get_validator_registration(pub_key).await.map(|entry| entry.inserted_at)
    }

    async fn set_proposer_duties(&self, proposer_duties: Vec<BuilderGetValidatorsResponseEntry>) -> Result<(), DatabaseError> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                INSERT INTO proposer_duties_archive SELECT * FROM proposer_duties ON CONFLICT (slot_number) DO NOTHING;
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

        let mut structured_params: Vec<(i32, i32, &[u8])> = Vec::with_capacity(proposer_duties.len());
        for entry in proposer_duties.iter() {
            structured_params.push((entry.slot as i32, entry.validator_index as i32, entry.entry.registration.message.public_key.as_ref()));
        }

        // Prepare the params vector from the structured parameters
        let params: Vec<&(dyn ToSql + Sync)> =
            structured_params.iter().flat_map(|tuple| vec![&tuple.0, &tuple.1, &tuple.2 as &(dyn ToSql + Sync)]).collect();

        // Construct the SQL statement with multiple VALUES clauses
        let mut sql = String::from("INSERT INTO proposer_duties (slot_number, validator_index, public_key) VALUES ");
        let num_params_per_row = 3;
        let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
            .map(|row| {
                let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
                format!("({})", placeholders.join(", "))
            })
            .collect();

        // Join the values clauses and append them to the SQL statement
        sql.push_str(&values_clauses.join(", "));
        sql.push_str(" ON CONFLICT (public_key) DO NOTHING");

        // Execute the query
        transaction.execute(&sql, &params[..]).await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn get_proposer_duties(&self) -> Result<Vec<BuilderGetValidatorsResponseEntry>, DatabaseError> {
        parse_rows(
            self.pool
                .get()
                .await?
                .query(
                    "
                            SELECT * FROM proposer_duties
                            INNER JOIN validator_registrations
                            ON proposer_duties.public_key = validator_registrations.public_key
                            INNER JOIN validator_preferences
                            ON proposer_duties.public_key = validator_preferences.public_key
                        ",
                    &[],
                )
                .await?,
        )
    }

    async fn set_known_validators(&self, known_validators: Vec<ValidatorSummary>) -> Result<(), DatabaseError> {
        info!("Known validators: current cache size: {:?}", self.known_validators_cache.len());

        let mut client = self.pool.get().await?;

        let new_keys_set: HashSet<BlsPublicKey> = known_validators.iter().map(|validator| validator.validator.public_key.clone()).collect();

        let old_keys_hash_set: HashSet<BlsPublicKey> = self
            .known_validators_cache
            .iter()
            .map(|ref_multi| ref_multi.key().clone()) // Access and clone the key from RefMulti
            .collect();

        let keys_to_add: Vec<BlsPublicKey> = new_keys_set.difference(&old_keys_hash_set).cloned().collect();
        let keys_to_remove: Vec<BlsPublicKey> = old_keys_hash_set.difference(&new_keys_set).cloned().collect();

        for key in &keys_to_add {
            self.known_validators_cache.insert(key.clone());
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
            let byte_keys: Vec<&[u8]> = chunk.iter().map(|k| k.as_ref()).collect();
            transaction.execute(sql, &[&byte_keys]).await?;
        }

        // Perform batch insertion
        for chunk in keys_to_add.chunks(10000) {
            let mut sql = String::from("INSERT INTO known_validators (public_key) VALUES ");
            let values_clauses: Vec<String> = (1..=chunk.len()).map(|i| format!("(${})", i)).collect();

            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (public_key) DO NOTHING");

            let mut structured_params: Vec<&[u8]> = Vec::new();
            for validator in chunk.iter() {
                structured_params.push(validator.as_ref());
            }

            let params: Vec<&(dyn ToSql + Sync)> = structured_params.iter().flat_map(|v| vec![v as &(dyn ToSql + Sync)]).collect();

            transaction.execute(&sql, &params[..]).await?;
        }

        transaction.commit().await?;

        Ok(())
    }

    async fn check_known_validators(&self, public_keys: Vec<BlsPublicKey>) -> Result<HashSet<BlsPublicKey>, DatabaseError> {
        let client = self.pool.get().await?;
        let mut pub_keys = HashSet::new();

        for public_key in public_keys.iter() {
            if self.known_validators_cache.contains(public_key) {
                pub_keys.insert(public_key.clone());
            } else {
                let rows = client.query("SELECT * FROM known_validators WHERE public_key = $1", &[&(public_key.as_ref())]).await?;
                for row in rows {
                    let public_key: BlsPublicKey = parse_bytes_to_pubkey(row.get::<&str, &[u8]>("public_key"))?;
                    self.known_validators_cache.insert(public_key.clone());
                    pub_keys.insert(public_key);
                }
            }
        }

        Ok(pub_keys)
    }

    async fn get_validator_pool_name(&self, api_key: &str) -> Result<Option<String>, DatabaseError> {
        let client = self.pool.get().await?;

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
        let rows = match client.query("SELECT * FROM validator_pools WHERE api_key = $1", &[&api_key]).await {
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

        Ok(Some(name))
    }

    async fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        payload_hash: &Hash32,
        message_received: u64,
        payload_fetched: u64,
    ) -> Result<(), DatabaseError> {
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
                    &(payload_hash.as_ref()),
                    &(slot as i32),
                    &(region_id),
                    &(proposer_pub_key.as_ref()),
                    &(message_received as i64),
                    &(payload_fetched as i64),
                ],
            )
            .await?;
        Ok(())
    }

    async fn save_delivered_payload(
        &self,
        bid_trace: &BidTrace,
        payload: Arc<PayloadAndBlobs>,
        latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        let region_id = self.region;
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        transaction.execute(
            "
                INSERT INTO delivered_payload 
                    (block_hash, payload_parent_hash, fee_recipient, state_root, receipts_root, logs_bloom, prev_randao, timestamp, block_number, gas_limit, gas_used, extra_data, base_fee_per_gas)
                VALUES 
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                ON CONFLICT (block_hash)
                DO NOTHING
            ",
            &[
                &(bid_trace.block_hash.as_ref()),
                &(payload.execution_payload.parent_hash().as_ref()),
                &(payload.execution_payload.fee_recipient().as_ref()),
                &(payload.execution_payload.state_root().as_ref()),
                &(payload.execution_payload.receipts_root().as_ref()),
                &(payload.execution_payload.logs_bloom().as_ref()),
                &(payload.execution_payload.prev_randao().as_ref()),
                &(payload.execution_payload.timestamp() as i64),
                &(payload.execution_payload.block_number() as i32),
                &(payload.execution_payload.gas_limit() as i32),
                &(payload.execution_payload.gas_used() as i32),
                &(payload.execution_payload.extra_data().as_ref()),
                &(PostgresNumeric::from(*payload.execution_payload.base_fee_per_gas())),
            ],
            ).await?;

        transaction
            .execute(
                "
                    INSERT INTO delivered_payload_preferences (block_hash, filtering, trusted_builders)
                    SELECT $1::bytea, filtering, trusted_builders
                    FROM validator_preferences
                    WHERE public_key = $2::bytea
                    ON CONFLICT (block_hash) DO NOTHING;                
                ",
                &[&(bid_trace.block_hash.as_ref()), &(bid_trace.proposer_public_key.as_ref())],
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
                &(bid_trace.block_hash.as_ref()),
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

        if !payload.execution_payload.transactions().is_empty() {
            // Save the transactions
            let mut structured_params: Vec<(&[u8], &[u8])> = Vec::new();
            for entry in payload.execution_payload.transactions().iter() {
                structured_params.push((payload.execution_payload.block_hash().as_ref(), entry.as_ref()));
            }

            // Prepare the params vector from the structured parameters
            let params: Vec<&(dyn ToSql + Sync)> =
                structured_params.iter().flat_map(|tuple| vec![&tuple.0 as &(dyn ToSql + Sync), &tuple.1 as &(dyn ToSql + Sync)]).collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO transaction (block_hash, bytes) VALUES ");
            let num_params_per_row = 2;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (md5(block_hash::text), md5(bytes::text)) DO NOTHING");

            transaction.execute(&sql, &params[..]).await?;
        }

        if payload.execution_payload.withdrawals().is_some() && !payload.execution_payload.withdrawals().unwrap().is_empty() {
            // Save the withdrawals
            let mut structured_params: Vec<(i32, &[u8], i32, &[u8], i64)> = Vec::new();
            for entry in payload.execution_payload.withdrawals().unwrap().iter() {
                structured_params.push((
                    entry.index as i32,
                    payload.execution_payload.block_hash().as_ref(),
                    entry.validator_index as i32,
                    entry.address.as_ref(),
                    entry.amount as i64,
                ));
            }

            // Prepare the params vector from the structured parameters
            let params: Vec<&(dyn ToSql + Sync)> = structured_params
                .iter()
                .flat_map(|tuple| vec![&tuple.0, &tuple.1 as &(dyn ToSql + Sync), &tuple.2, &tuple.3 as &(dyn ToSql + Sync), &tuple.4])
                .collect();

            // Construct the SQL statement with multiple VALUES clauses
            let mut sql = String::from("INSERT INTO withdrawal (index, block_hash, validator_index, address, amount) VALUES ");
            let num_params_per_row = 5;
            let values_clauses: Vec<String> = (0..params.len() / num_params_per_row)
                .map(|row| {
                    let placeholders: Vec<String> = (1..=num_params_per_row).map(|n| format!("${}", row * num_params_per_row + n)).collect();
                    format!("({})", placeholders.join(", "))
                })
                .collect();

            // Join the values clauses and append them to the SQL statement
            sql.push_str(&values_clauses.join(", "));
            sql.push_str(" ON CONFLICT (index, block_hash) DO NOTHING");

            transaction.execute(&sql, &params[..]).await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    async fn store_block_submission(
        &self,
        submission: Arc<SignedBidSubmission>,
        trace: Arc<SubmissionTrace>,
        optimistic_version: i16,
    ) -> Result<(), DatabaseError> {
        let region_id = self.region;
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction.execute(
            "
                INSERT INTO
                    block_submission (block_number, slot_number, parent_hash, block_hash, builder_pubkey, proposer_pubkey, proposer_fee_recipient, gas_limit, gas_used, value, num_txs, timestamp, first_seen)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                ON CONFLICT (block_hash)
                DO UPDATE SET
                    first_seen = LEAST(block_submission.first_seen, excluded.first_seen)
                ",
            &[
                &(submission.block_number() as i32),
                &(submission.slot() as i32),
                &(submission.parent_hash().as_ref()),
                &(submission.block_hash().as_ref()),
                &(submission.builder_public_key().as_ref()),
                &(submission.proposer_public_key().as_ref()),
                &(submission.proposer_fee_recipient().as_ref()),
                &(submission.gas_limit() as i32),
                &(submission.gas_used() as i32),
                &(PostgresNumeric::from(submission.value())),
                &(submission.transactions().len() as i32),
                &(submission.timestamp() as i64),
                &(trace.receive as i64),
            ],
        ).await?;

        transaction.execute(
            "
                INSERT INTO
                    submission_trace (block_hash, region_id, optimistic_version, receive, decode, pre_checks, signature, floor_bid_checks, simulation, auctioneer_update, request_finish)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ",
            &[
                &(submission.block_hash().as_ref()),
                &(region_id),
                &(optimistic_version),
                &(trace.receive as i64),
                &(trace.decode as i64),
                &(trace.pre_checks as i64),
                &(trace.signature as i64),
                &(trace.floor_bid_checks as i64),
                &(trace.simulation as i64),
                &(trace.auctioneer_update as i64),
                &(trace.request_finish as i64),
            ],
        ).await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn store_builder_info(&self, builder_pub_key: &BlsPublicKey, builder_info: BuilderInfo) -> Result<(), DatabaseError> {
        self.pool
            .get()
            .await?
            .execute(
                "
                    INSERT INTO builder_info (public_key, collateral, is_optimistic, builder_id)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (public_key)
                    DO UPDATE SET
                        collateral = excluded.collateral,
                        is_optimistic = excluded.is_optimistic
                ",
                &[
                    &(builder_pub_key.as_ref()),
                    &(PostgresNumeric::from(builder_info.collateral)),
                    &(builder_info.is_optimistic),
                    &(builder_info.builder_id),
                ],
            )
            .await?;

        Ok(())
    }

    async fn db_get_builder_info(&self, builder_pub_key: &BlsPublicKey) -> Result<BuilderInfo, DatabaseError> {
        match self
            .pool
            .get()
            .await?
            .query(
                "
                    SELECT * FROM builder_info 
                    WHERE public_key = $1
                ",
                &[&(builder_pub_key.as_ref())],
            )
            .await?
        {
            rows if rows.is_empty() => Err(DatabaseError::BuilderInfoNotFound { public_key: builder_pub_key.clone() }),
            rows => parse_row(rows.first().unwrap()),
        }
    }

    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError> {
        parse_rows(self.pool.get().await?.query("SELECT * FROM builder_info", &[]).await?)
    }

    async fn check_builder_api_key(&self, api_key: &str) -> Result<bool, DatabaseError> {
        let client = self.pool.get().await?;
        let rows = client.query("SELECT * FROM builder_info WHERE api_key = $1", &[&(api_key)]).await?;

        Ok(!rows.is_empty())
    }

    async fn db_demote_builder(&self, builder_pub_key: &BlsPublicKey, block_hash: &Hash32, reason: String) -> Result<(), DatabaseError> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        transaction
            .execute(
                "
                    UPDATE builder_info 
                    SET is_optimistic = FALSE 
                    WHERE public_key = $1
                ",
                &[&(builder_pub_key.as_ref())],
            )
            .await?;

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        transaction
            .execute(
                "
                    INSERT INTO demotions (public_key, block_hash, demotion_time, reason)
                    VALUES ($1, $2, $3, $4)
                ",
                &[&(builder_pub_key.as_ref()), &(block_hash.as_ref()), &(timestamp as i64), &(reason)],
            )
            .await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn save_simulation_result(&self, block_hash: ByteVector<32>, block_sim_result: Result<(), BlockSimError>) -> Result<(), DatabaseError> {
        if let Err(e) = block_sim_result {
            self.pool
                .get()
                .await?
                .execute(
                    "
                        INSERT INTO simulation_error (block_hash, error)
                        VALUES ($1, $2)
                        ON CONFLICT (block_hash)
                        DO NOTHING
                    ",
                    &[&(block_hash.as_ref()), &(format!("{:?}", e))],
                )
                .await?;
        }
        Ok(())
    }

    async fn get_bids(&self, filters: &BidFilters) -> Result<Vec<BidSubmissionDocument>, DatabaseError> {
        let filters = PgBidFilters::from(filters);

        let mut query = String::from(
            "
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
            WHERE 1 = 1
        ",
        );

        let mut param_index = 1;
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

        if let Some(slot) = filters.slot() {
            query.push_str(&format!(" AND block_submission.slot_number = ${}", param_index));
            params.push(Box::new(slot));
            param_index += 1;
        }

        if let Some(block_number) = filters.block_number() {
            query.push_str(&format!(" AND block_submission.block_number = ${}", param_index));
            params.push(Box::new(block_number));
            param_index += 1;
        }

        if let Some(proposer_pubkey) = filters.proposer_pubkey() {
            query.push_str(&format!(" AND block_submission.proposer_pubkey = ${}", param_index));
            params.push(Box::new(proposer_pubkey));
            param_index += 1;
        }

        if let Some(builder_pubkey) = filters.builder_pubkey() {
            query.push_str(&format!(" AND block_submission.builder_pubkey = ${}", param_index));
            params.push(Box::new(builder_pubkey));
            param_index += 1;
        }

        if let Some(block_hash) = filters.block_hash() {
            query.push_str(&format!(" AND block_submission.block_hash = ${}", param_index));
            params.push(Box::new(block_hash));
            param_index += 1;
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        parse_rows(self.pool.get().await?.query(&query, &params_refs[..]).await?)
    }

    async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
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
                delivered_payload 
            INNER JOIN
                block_submission 
            ON 
                block_submission.block_hash = delivered_payload.block_hash
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
            query.push_str(&format!(" AND block_submission.slot_number = ${}", param_index));
            params.push(Box::new(slot));
            param_index += 1;
        }

        if let Some(cursor) = filters.cursor() {
            query.push_str(&format!(" AND block_submission.slot_number <= ${}", param_index));
            params.push(Box::new(cursor));
            param_index += 1;
        }

        if let Some(block_number) = filters.block_number() {
            query.push_str(&format!(" AND block_submission.block_number = ${}", param_index));
            params.push(Box::new(block_number));
            param_index += 1;
        }

        if let Some(proposer_pubkey) = filters.proposer_pubkey() {
            query.push_str(&format!(" AND block_submission.proposer_pubkey = ${}", param_index));
            params.push(Box::new(proposer_pubkey));
            param_index += 1;
        }

        if let Some(builder_pubkey) = filters.builder_pubkey() {
            query.push_str(&format!(" AND block_submission.builder_pubkey = ${}", param_index));
            params.push(Box::new(builder_pubkey));
            param_index += 1;
        }

        if let Some(block_hash) = filters.block_hash() {
            query.push_str(&format!(" AND block_submission.block_hash = ${}", param_index));
            params.push(Box::new(block_hash));
            param_index += 1;
        }

        if let Some(filtering) = filtering {
            query.push_str(&format!(" AND delivered_payload_preferences.filtering = ${}", param_index));
            params.push(Box::new(filtering));
            param_index += 1;
        }

        if let Some(trusted_builders) = &validator_preferences.trusted_builders {
            query.push_str(&format!(" AND delivered_payload_preferences.trusted_builders @> ${}", param_index));
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
            query.push_str(&format!(" LIMIT ${}", param_index));
            params.push(Box::new(limit));
        }

        let params_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect();

        parse_rows(self.pool.get().await?.query(&query, &params_refs[..]).await?)
    }

    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: ByteVector<32>,
        public_key: BlsPublicKey,
        best_block_hash: ByteVector<32>,
        trace: GetHeaderTrace,
    ) -> Result<(), DatabaseError> {
        let region_id = self.region;

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                    INSERT INTO get_header
                        (slot_number, region_id, parent_hash, proposer_pubkey, block_hash)
                    VALUES
                        ($1, $2, $3, $4, $5)
                ",
                &[&(slot as i32), &(region_id), &(parent_hash.as_ref()), &(public_key.as_ref()), &(best_block_hash.as_ref())],
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
                    &(best_block_hash.as_ref()),
                    &(region_id),
                    &(trace.receive as i64),
                    &(trace.validation_complete as i64),
                    &(trace.best_bid_fetched as i64),
                ],
            )
            .await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn save_failed_get_payload(
        &self,
        slot: u64,
        block_hash: ByteVector<32>,
        error: String,
        trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        let region_id = self.region;

        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction
            .execute(
                "
                    INSERT INTO failed_payload
                        (region_id, slot_number, block_hash, error)
                    VALUES
                        ($1, $2, $3, $4)
                ",
                &[&(region_id), &(slot as i32), &(block_hash.as_ref()), &(error)],
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
                &(block_hash.as_ref()),
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

        Ok(())
    }

    async fn store_header_submission(&self, submission: Arc<SignedHeaderSubmission>, trace: Arc<HeaderSubmissionTrace>) -> Result<(), DatabaseError> {
        let region_id = self.region;
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;

        transaction.execute(
            "
                INSERT INTO
                    header_submission (block_number, slot_number, parent_hash, block_hash, builder_pubkey, proposer_pubkey, proposer_fee_recipient, gas_limit, gas_used, value, timestamp, first_seen)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                ON CONFLICT (block_hash)
                DO UPDATE SET
                    first_seen = LEAST(header_submission.first_seen, excluded.first_seen)
                ",
            &[
                &(submission.execution_payload_header().block_number() as i32),
                &(submission.slot() as i32),
                &(submission.parent_hash().as_ref()),
                &(submission.block_hash().as_ref()),
                &(submission.builder_public_key().as_ref()),
                &(submission.proposer_public_key().as_ref()),
                &(submission.proposer_fee_recipient().as_ref()),
                &(submission.gas_limit() as i32),
                &(submission.gas_used() as i32),
                &(PostgresNumeric::from(submission.value())),
                &(submission.timestamp() as i64),
                &(trace.receive as i64),
            ],
        ).await?;

        transaction.execute(
            "
                INSERT INTO
                    header_submission_trace (block_hash, region_id, receive, decode, pre_checks, signature, floor_bid_checks, auctioneer_update, request_finish)
                VALUES
                    ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ",
            &[
                &(submission.block_hash().as_ref()),
                &(region_id),
                &(trace.receive as i64),
                &(trace.decode as i64),
                &(trace.pre_checks as i64),
                &(trace.signature as i64),
                &(trace.floor_bid_checks as i64),
                &(trace.auctioneer_update as i64),
                &(trace.request_finish as i64),
            ],
        ).await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn save_gossiped_header_trace(&self, block_hash: ByteVector<32>, trace: Arc<GossipedHeaderTrace>) -> Result<(), DatabaseError> {
        let region_id = self.region;

        self.pool
            .get()
            .await?
            .execute(
                "
                INSERT INTO
                    gossiped_header_trace (block_hash, region_id, on_receive, on_gossip_receive, pre_checks, auctioneer_update)
                VALUES
                    ($1, $2, $3, $4, $5, $6)
            ",
                &[
                    &(block_hash.as_ref()),
                    &(region_id),
                    &(trace.on_receive as i64),
                    &(trace.on_gossip_receive as i64),
                    &(trace.pre_checks as i64),
                    &(trace.auctioneer_update as i64),
                ],
            )
            .await?;
        Ok(())
    }

    async fn save_gossiped_payload_trace(&self, block_hash: ByteVector<32>, trace: Arc<GossipedPayloadTrace>) -> Result<(), DatabaseError> {
        let region_id = self.region;

        self.pool
            .get()
            .await?
            .execute(
                "
                INSERT INTO
                    gossiped_payload_trace (block_hash, region_id, receive, pre_checks, auctioneer_update)
                VALUES
                    ($1, $2, $3, $4, $5)
            ",
                &[&(block_hash.as_ref()), &(region_id), &(trace.receive as i64), &(trace.pre_checks as i64), &(trace.auctioneer_update as i64)],
            )
            .await?;
        Ok(())
    }

    async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError> {
        parse_rows(
            self.pool
                .get()
                .await?
                .query(
                    "
                    SELECT * FROM trusted_proposers 
                ",
                    &[],
                )
                .await?,
        )
    }
}
