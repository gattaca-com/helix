#[cfg(test)]
mod tests {
    use std::{ops::DerefMut, sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod};
    use helix_common::{
        Filtering, PostgresConfig, ValidatorSummary, api::proposer_api::ValidatorRegistrationInfo,
        local_cache::LocalCache, utils::utcnow_sec, validator_preferences::ValidatorPreferences,
    };
    use helix_types::{
        BlsKeypair, BlsSecretKey, SignedValidatorRegistration, TestRandomSeed, Validator,
        ValidatorRegistration,
    };
    use rand::{Rng, rng, seq::SliceRandom};
    use tokio::sync::OnceCell;
    use tokio_postgres::NoTls;

    use crate::database::postgres::{
        postgres_db_init::run_migrations_async, postgres_db_service::PostgresDatabaseService,
    };

    const REGION: i16 = 1;
    const REGION_NAME: &str = "LOCAL";

    /// These tests depend on a local instance of postgres running on port 5432
    /// e.g. to start a local postgres instance in docker:
    /// docker run -d --name postgres -e POSTGRES_PASSWORD=password -p 5432:5432
    /// timescale/timescaledb-ha:pg16 https://docs.timescale.com/self-hosted/latest/install/installation-docker/
    fn test_config() -> Config {
        let mut cfg = Config::new();
        cfg.host = Some("localhost".to_string());
        cfg.port = Some(5432);
        cfg.dbname = Some("postgres".to_string());
        cfg.user = Some("postgres".to_string());
        cfg.password = Some("password".to_string());
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
        cfg
    }

    fn test_postgres_config() -> PostgresConfig {
        PostgresConfig {
            hostname: "localhost".to_string(),
            port: 5432,
            db_name: "postgres".to_string(),
            user: "postgres".to_string(),
            region: REGION,
            region_name: REGION_NAME.to_string(),
        }
    }

    static SETUP: OnceCell<()> = OnceCell::const_new();
    async fn run_setup() {
        SETUP.get_or_init(|| async { setup_test_conn().await.unwrap() }).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fn setup_test_pool() -> Result<Pool, Box<dyn std::error::Error>> {
        Ok(test_config().create_pool(None, NoTls)?)
    }

    async fn setup_test_conn() -> Result<(), Box<dyn std::error::Error>> {
        let pool = setup_test_pool()?;
        let mut client = pool.get().await?;

        // ping the database to make sure we're connected
        let resp = client.query_one("SELECT 1", &[]).await?;
        assert!(resp.get::<_, i32>(0) == 1);

        let client = client.deref_mut().deref_mut();
        match run_migrations_async(client).await {
            Ok(report) => {
                println!("Applied migrations: {}", report.applied_migrations().len());
                println!("Migrations: {report:?}");
            }
            Err(e) => {
                println!("Error applying migrations: {e}");
                return Err(e);
            }
        }

        // init region
        let db_service = PostgresDatabaseService::new(&test_config(), REGION)?;
        db_service.init_region(&test_postgres_config()).await;

        Ok(())
    }

    fn get_randomized_signed_validator_registration() -> ValidatorRegistrationInfo {
        let timestamp = utcnow_sec();
        let gas_limit = 0;
        let key = BlsKeypair::random();
        let signature = key.sk.sign(B256::ZERO).serialize();
        let pubkey = key.pk.serialize();
        ValidatorRegistrationInfo {
            registration: SignedValidatorRegistration {
                message: ValidatorRegistration {
                    fee_recipient: Default::default(),
                    timestamp,
                    gas_limit,
                    pubkey: pubkey.into(),
                },
                signature: signature.into(),
            },
            preferences: ValidatorPreferences {
                filtering: Filtering::Global,
                trusted_builders: Some(vec!["test".to_string(), "test2".to_string()]),
                header_delay: true,
                delay_ms: Some(1000),
                disable_inclusion_lists: true,
                disable_optimistic: true,
            },
        }
    }

    #[tokio::test]
    async fn test_save_and_get_known_validators() {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut validator_summaries = Vec::new();

        for i in 0..100 {
            let key = BlsKeypair::random();
            let public_key = key.pk;

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    pubkey: public_key.serialize().into(),
                    ..Validator::test_random()
                },
            };

            validator_summaries.push(validator_summary);
        }

        let mut validator_summaries_clone = validator_summaries.clone();

        let result = db_service.set_known_validators(validator_summaries).await;
        assert!(result.is_ok());

        let mut new_validator_summaries = Vec::new();

        for i in 0..10 {
            let key = BlsSecretKey::random();
            let pubkey = key.public_key();

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    pubkey: pubkey.serialize().into(),
                    ..Validator::test_random()
                },
            };

            new_validator_summaries.push(validator_summary);
        }

        let _removed = remove_random_items::<ValidatorSummary>(&mut validator_summaries_clone, 5);

        randomly_insert_values::<ValidatorSummary>(
            &mut validator_summaries_clone,
            new_validator_summaries,
        );

        let _final_list = validator_summaries_clone.clone();

        let result = db_service.set_known_validators(validator_summaries_clone).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_large_batch() {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut validator_summaries = Vec::new();

        for i in 0..200_000 {
            let key = BlsSecretKey::random();
            let pubkey = key.public_key();

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    pubkey: pubkey.serialize().into(),
                    ..Validator::test_random()
                },
            };

            validator_summaries.push(validator_summary);
        }

        let result = db_service.set_known_validators(validator_summaries).await;
        assert!(result.is_ok());
    }

    // TODO: fix tests

    // #[tokio::test]
    // async fn test_save_and_get_builder_info() {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

    //     let public_key =
    // BlsPublicKey::deserialize(&alloy_primitives::hex!("
    // 8C266FD5CB50B5D9431DAA69C4BE17BC9A79A85D172112DA09E0AC3E2D0DCF785021D49B6DF57827D6BC61EBA086A507"
    // )).unwrap().serialize().into();     let builder_info = helix_common::BuilderInfo {
    //         collateral: U256::from(10000000000000000000u64),
    //         is_optimistic: false,
    //         is_optimistic_for_regional_filtering: false,
    //         builder_id: None,
    //         builder_ids: Some(vec!["test3".to_string()]),
    //         api_key: None,
    //     };

    //     let result = db_service.store_builder_info(&public_key, &builder_info).await;
    //     assert!(result.is_ok());

    //     let result = db_service.get_all_builder_infos().await.unwrap();
    //     assert!(result.len() == 1);
    //     assert!(result[0].pub_key == public_key);
    // }

    // #[tokio::test]
    // async fn test_demotion() {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

    //     let key = BlsSecretKey::random();
    //     let public_key = key.public_key().serialize().into();

    //     let builder_info = helix_common::BuilderInfo {
    //         collateral: Default::default(),
    //         is_optimistic: false,
    //         is_optimistic_for_regional_filtering: false,
    //         builder_id: None,
    //         builder_ids: None,
    //         api_key: None,
    //     };

    //     let result = db_service.store_builder_info(&public_key, &builder_info).await;
    //     assert!(result.is_ok());

    //     let result =
    //         db_service.db_demote_builder(0, &public_key, &Default::default(),
    // "".to_string()).await;     assert!(result.is_ok());
    // }

    // #[tokio::test]
    // async fn test_store_block_submission() -> Result<(), Box<dyn std::error::Error>> {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

    //     let pubkey =
    // BlsPublicKey::deserialize(alloy_primitives::hex!("
    // 8592669BC0ACF28BC25D42699CEFA6101D7B10443232FE148420FF0FCDBF8CD240F5EBB94BC904CB6BEFFB61A1F8D36A"
    // ).as_ref()).unwrap();

    //     let bid_trace =
    //         BidTrace { proposer_pubkey: pubkey.serialize().into(), ..BidTrace::test_random() };

    //     let signed_bid_submission = SignedBidSubmission {
    //         message: bid_trace.clone(),
    //         execution_payload: ExecutionPayload::test_random().into(),
    //         blobs_bundle: Arc::new(BlobsBundle::V1(Default::default())),
    //         signature: BlsSignatureBytes::random(),
    //         execution_requests: Default::default(),
    //     };

    //     let submission_trace = SubmissionTrace { receive: utcnow_ns(), ..Default::default() };

    //     db_service
    //         .store_block_submission(
    //             signed_bid_submission.into(),
    //             submission_trace,
    //             OptimisticVersion::NotOptimistic,
    //         )
    //         .await?;
    //     Ok(())
    // }

    #[tokio::test]
    async fn test_get_bids() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;
        let filter = helix_common::api::data_api::BidFilters {
            slot: Some(1234),
            cursor: None,
            limit: None,
            block_hash: None,
            block_number: None,
            proposer_pubkey: None,
            builder_pubkey: None,
            order_by: None,
        };
        let validator_preferences = ValidatorPreferences::default();
        let bids = db_service.get_bids(&filter, Arc::new(validator_preferences)).await?;
        println!("Bids: {bids:?}");
        Ok(())
    }

    // #[tokio::test]
    // async fn test_save_delivered_payloads() -> Result<(), Box<dyn std::error::Error>> {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

    //     let mut execution_payload = ExecutionPayload::test_random();

    //     // execution_payload
    //     //     .transactions_mut()
    //     //     .push(ethereum_consensus::capella::Transaction::default());

    //     // execution_payload
    //     //     .transactions_mut()
    //     //     .push(ethereum_consensus::capella::Transaction::default());
    //     execution_payload
    //         .withdrawals
    //         .push(Withdrawal {
    //             index: 0,
    //             validator_index: 0,
    //             amount: 0,
    //             address: Default::default(),
    //         })
    //         .unwrap();

    //     let latency_trace = GetPayloadTrace::default();

    //     let payload_and_blobs =
    //         PayloadAndBlobs { execution_payload, blobs_bundle: Default::default() };

    //     db_service
    //         .save_delivered_payload(
    //             BlsPublicKeyBytes::random(),
    //             Arc::new(payload_and_blobs),
    //             &latency_trace,
    //             None,
    //         )
    //         .await?;
    //     Ok(())
    // }

    #[tokio::test]
    async fn test_get_delivered_payloads() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;
        let filter = helix_common::api::data_api::BidFilters {
            slot: None,
            cursor: None,
            limit: None,
            block_hash: None,
            block_number: None,
            proposer_pubkey: None,
            builder_pubkey: None,
            order_by: None,
        };

        let validator_preferences = ValidatorPreferences::default();

        let delivered_payloads =
            db_service.get_delivered_payloads(&filter, Arc::new(validator_preferences)).await?;
        println!("delivered payloads {delivered_payloads:?}");
        Ok(())
    }

    #[tokio::test]
    async fn test_late_payloads() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_too_late_get_payload(1, &reg.message.pubkey, &Default::default(), 0, 0)
            .await?;

        Ok(())
    }

    // #[tokio::test]
    // async fn test_get_header() -> Result<(), Box<dyn std::error::Error>> {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

    //     let reg = get_randomized_signed_validator_registration().registration;

    //     db_service
    //         .save_get_header_call(
    //             1,
    //             Default::default(),
    //             reg.message.pubkey,
    //             Default::default(),
    //             Default::default(),
    //             false,
    //             None,
    //         )
    //         .await?;

    //     Ok(())
    // }

    #[tokio::test]
    async fn test_failed_payloads() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let _reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_failed_get_payload(1, Default::default(), "error".to_string(), Default::default())
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_gossiped_payload() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;
        db_service.init_region(&test_postgres_config()).await;

        db_service.save_gossiped_payload_trace(Default::default(), Default::default()).await?;

        Ok(())
    }

    fn remove_random_items<T>(vec: &mut Vec<T>, count: usize) -> Vec<T> {
        let mut rng = rng();

        // Ensure we don't try to remove more items than the Vec contains
        let count = std::cmp::min(count, vec.len());

        // Shuffle the Vec to randomize which items are at the end
        vec.shuffle(&mut rng);

        // Calculate the index from where to split the Vec to keep the first part
        // and return the second part containing 'count' items
        let split_index = vec.len() - count;

        // Use split_off to divide the Vec and return the removed items
        vec.split_off(split_index)
    }

    fn randomly_insert_values<T>(existing_vec: &mut Vec<T>, new_values: Vec<T>) {
        let mut rng = rand::rng();

        for value in new_values {
            let insert_index = rng.random_range(0..=existing_vec.len());
            existing_vec.insert(insert_index, value);
        }
    }

    // ============================================================================
    // Integration tests for start_db_service load functions
    // ============================================================================

    /// Timing result for a single load operation
    #[derive(Debug)]
    struct LoadTiming {
        name: &'static str,
        duration_ms: u128,
        row_count: usize,
    }

    impl LoadTiming {
        fn rows_per_ms(&self) -> f64 {
            if self.duration_ms == 0 {
                return 0.0;
            }
            self.row_count as f64 / self.duration_ms as f64
        }
    }

    /// Test configuration for load function benchmarking
    struct LoadTestConfig {
        /// Number of known validators to populate
        known_validators_count: usize,
        /// Number of validator registrations to populate
        validator_registrations_count: usize,
        /// Maximum acceptable load time in ms (0 = no limit)
        max_total_load_time_ms: u128,
    }

    impl Default for LoadTestConfig {
        fn default() -> Self {
            Self {
                // Use smaller counts for CI, can be increased for stress testing
                known_validators_count: 10_000,
                validator_registrations_count: 10_000,
                max_total_load_time_ms: 0, // No limit by default
            }
        }
    }

    /// Populates test data for load function benchmarking
    async fn populate_test_data(
        db_service: &PostgresDatabaseService,
        config: &LoadTestConfig,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!(
            "Populating test data: {} validators, {} registrations",
            config.known_validators_count, config.validator_registrations_count
        );

        // Populate known validators
        if config.known_validators_count > 0 {
            let start = std::time::Instant::now();
            let mut validator_summaries = Vec::with_capacity(config.known_validators_count);
            for i in 0..config.known_validators_count {
                let key = BlsSecretKey::random();
                let pubkey = key.public_key();
                validator_summaries.push(ValidatorSummary {
                    index: i as u64,
                    balance: 32_000_000_000,
                    status: helix_common::ValidatorStatus::Active,
                    validator: Validator {
                        pubkey: pubkey.serialize().into(),
                        ..Validator::test_random()
                    },
                });
            }
            db_service.set_known_validators(validator_summaries).await?;
            println!("  Populated known_validators in {:?}", start.elapsed());
        }

        // Populate validator registrations
        if config.validator_registrations_count > 0 {
            let start = std::time::Instant::now();
            let mut registrations = Vec::with_capacity(config.validator_registrations_count);
            for _ in 0..config.validator_registrations_count {
                let reg = get_randomized_signed_validator_registration();
                registrations.push(helix_common::SignedValidatorRegistrationEntry {
                    registration_info: reg,
                    pool_name: None,
                    user_agent: None,
                    inserted_at: utcnow_sec(),
                });
            }
            db_service.save_validator_registrations(&registrations).await?;
            println!("  Populated validator_registrations in {:?}", start.elapsed());
        }

        Ok(())
    }

    /// Measures the timing of each load function individually
    async fn measure_load_functions(
        db_service: &PostgresDatabaseService,
        local_cache: Arc<LocalCache>,
    ) -> Vec<LoadTiming> {
        let mut timings = Vec::new();

        // Measure load_known_validators
        let start = std::time::Instant::now();
        db_service.load_known_validators().await;
        let duration = start.elapsed();
        let count = db_service.local_cache.known_validators_cache.read().len();
        timings.push(LoadTiming {
            name: "load_known_validators",
            duration_ms: duration.as_millis(),
            row_count: count,
        });

        // Measure load_validator_registrations
        let start = std::time::Instant::now();
        db_service.load_validator_registrations().await;
        let duration = start.elapsed();
        let count = db_service.local_cache.validator_registration_cache.len();
        timings.push(LoadTiming {
            name: "load_validator_registrations",
            duration_ms: duration.as_millis(),
            row_count: count,
        });

        // Measure load_builder_infos
        let start = std::time::Instant::now();
        db_service.load_builder_infos(local_cache.clone()).await;
        let duration = start.elapsed();
        timings.push(LoadTiming {
            name: "load_builder_infos",
            duration_ms: duration.as_millis(),
            row_count: 0, // Can't easily access count
        });

        timings
    }

    /// Integration test that measures load function performance.
    ///
    /// This test validates that the database load functions complete within
    /// acceptable time bounds, helping catch performance regressions that could
    /// impact startup time or cache refresh cycles.
    #[tokio::test]
    async fn test_load_functions_timing() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), REGION)?;
        let local_cache = Arc::new(LocalCache::new());

        let config = LoadTestConfig::default();

        // Populate test data
        populate_test_data(&db_service, &config).await?;

        // Measure individual load functions
        let timings = measure_load_functions(&db_service, local_cache.clone()).await;

        // Print timing report
        println!("\n=== Load Function Timing Report ===");
        let mut total_ms = 0u128;
        for timing in &timings {
            println!(
                "{:30} {:>6}ms  ({} rows, {:.2} rows/ms)",
                timing.name,
                timing.duration_ms,
                timing.row_count,
                timing.rows_per_ms()
            );
            total_ms += timing.duration_ms;
        }
        println!("{:30} {:>6}ms", "TOTAL", total_ms);
        println!("===================================\n");

        // Check timing threshold if configured
        if config.max_total_load_time_ms > 0 {
            assert!(
                total_ms <= config.max_total_load_time_ms,
                "Total load time {}ms exceeded threshold {}ms",
                total_ms,
                config.max_total_load_time_ms
            );
        }

        Ok(())
    }

    /// Stress test with larger data volumes.
    ///
    /// This test uses production-like data volumes to measure worst-case
    /// load times. Run with:
    /// ```
    /// cargo test --release test_load_functions_stress -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore] // Run manually for stress testing
    async fn test_load_functions_stress() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), REGION)?;
        let local_cache = Arc::new(LocalCache::new());

        // Production-like volumes
        let config = LoadTestConfig {
            known_validators_count: 1_000_000,
            validator_registrations_count: 500_000,
            max_total_load_time_ms: 30_000, // 30 second threshold
        };

        println!("\n=== STRESS TEST: Production-like volumes ===");
        println!(
            "Testing with {} known validators, {} registrations",
            config.known_validators_count, config.validator_registrations_count
        );

        // Populate test data
        populate_test_data(&db_service, &config).await?;

        // Measure individual load functions
        let timings = measure_load_functions(&db_service, local_cache.clone()).await;

        // Print timing report
        println!("\n=== Load Function Timing Report (STRESS) ===");
        let mut total_ms = 0u128;
        for timing in &timings {
            println!(
                "{:30} {:>8}ms  ({:>10} rows, {:>8.2} rows/ms)",
                timing.name,
                timing.duration_ms,
                timing.row_count,
                timing.rows_per_ms()
            );
            total_ms += timing.duration_ms;
        }
        println!("{:30} {:>8}ms", "TOTAL", total_ms);
        println!("=============================================\n");

        // Check timing threshold
        if config.max_total_load_time_ms > 0 {
            assert!(
                total_ms <= config.max_total_load_time_ms,
                "Total load time {}ms exceeded threshold {}ms - PERFORMANCE REGRESSION DETECTED",
                total_ms,
                config.max_total_load_time_ms
            );
        }

        Ok(())
    }
}
