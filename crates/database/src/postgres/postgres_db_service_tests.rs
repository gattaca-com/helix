#[cfg(test)]
mod tests {
    use std::{default::Default, ops::DerefMut, sync::Arc, time::Duration};

    use alloy_primitives::{B256, U256};
    use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod};
    use helix_common::{
        api::proposer_api::ValidatorRegistrationInfo,
        bid_submission::OptimisticVersion,
        utils::{utcnow_ns, utcnow_sec},
        validator_preferences::ValidatorPreferences,
        Filtering, GetPayloadTrace, PostgresConfig, SubmissionTrace, ValidatorSummary,
    };
    use helix_types::{
        BidTrace, BlobsBundle, BlsKeypair, BlsPublicKey, BlsPublicKeyBytes, BlsSecretKey,
        BlsSignatureBytes, ExecutionPayload, PayloadAndBlobs, SignedBidSubmissionElectra,
        SignedValidatorRegistration, TestRandomSeed, Validator, ValidatorRegistration, Withdrawal,
    };
    use rand::{rng, seq::SliceRandom, Rng};
    use tokio::sync::OnceCell;
    use tokio_postgres::NoTls;

    use crate::{
        postgres::{
            postgres_db_init::run_migrations_async, postgres_db_service::PostgresDatabaseService,
        },
        DatabaseService,
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

    // TODO: cleanup config
    fn test_postgres_config() -> PostgresConfig {
        PostgresConfig {
            hostname: "localhost".to_string(),
            port: 5432,
            db_name: "postgres".to_string(),
            user: "postgres".to_string(),
            password: "password".to_string(),
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
            },
        }
    }

    // #[tokio::test]
    // async fn test_save_and_get_validator_registrations() {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
    //     db_service.start_registration_processor().await;

    //     const NUM_REGISTRATIONS: usize = 2;

    //     let registrations = (0..NUM_REGISTRATIONS)
    //         .map(|_| get_randomized_signed_validator_registration())
    //         .collect::<Vec<_>>();

    //     db_service
    //         .save_validator_registrations(registrations.clone(), Some("test".to_string()), None)
    //         .await
    //         .unwrap();
    //     sleep(Duration::from_secs(5)).await;

    //     for registration in registrations {
    //         let result = db_service
    //             .get_validator_registration(&registration.registration.message.pubkey)
    //             .await
    //             .unwrap();
    //         assert_eq!(
    //             result.registration_info.registration.signature,
    //             registration.registration.signature
    //         );
    //     }
    // }

    // #[tokio::test]
    // async fn test_save_and_get_validator_registrations_for_pub_keys() {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
    //     db_service.start_registration_processor().await;

    //     const N_REGISTRATIONS: usize = 2;

    //     let registrations = (0..N_REGISTRATIONS)
    //         .map(|_| get_randomized_signed_validator_registration())
    //         .collect::<Vec<_>>();

    //     db_service
    //         .save_validator_registrations(registrations.clone(), Some("test".to_string()), None)
    //         .await
    //         .unwrap();

    //     sleep(Duration::from_secs(5)).await;

    //     let result = db_service
    //         .get_validator_registrations_for_pub_keys(
    //             registrations
    //                 .iter()
    //                 .map(|r| &r.registration.message.pubkey)
    //                 .collect::<Vec<_>>()
    //                 .as_slice(),
    //         )
    //         .await
    //         .unwrap();

    //     for registration in registrations {
    //         let result = result
    //             .iter()
    //             .find(|r| {
    //                 r.registration_info.registration.message.pubkey
    //                     == registration.registration.message.pubkey
    //             })
    //             .unwrap();
    //         assert_eq!(
    //             result.registration_info.registration.signature,
    //             registration.registration.signature
    //         );
    //     }
    // }

    // #[tokio::test]
    // async fn test_save_and_get_proposer_duties() {
    //     run_setup().await;

    //     let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
    //     let mut proposer_duties = Vec::new();
    //     for i in 0..10 {
    //         let registration = get_randomized_signed_validator_registration();
    //         db_service
    //             .save_validator_registrations(
    //                 vec![registration.clone()],
    //                 Some("test".to_string()),
    //                 None,
    //             )
    //             .await
    //             .unwrap();

    //         proposer_duties.push(BuilderGetValidatorsResponseEntry {
    //             slot: i.into(),
    //             validator_index: i,
    //             entry: registration.clone(),
    //         });
    //     }

    //     let result = db_service.set_proposer_duties(proposer_duties).await;
    //     assert!(result.is_ok());

    //     let result = db_service.get_proposer_duties().await;
    //     assert!(result.is_ok());
    // }

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

    #[tokio::test]
    async fn test_save_and_get_builder_info() {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let public_key = BlsPublicKey::deserialize(&alloy_primitives::hex!("8C266FD5CB50B5D9431DAA69C4BE17BC9A79A85D172112DA09E0AC3E2D0DCF785021D49B6DF57827D6BC61EBA086A507")).unwrap().serialize().into();
        let builder_info = helix_common::BuilderInfo {
            collateral: U256::from(10000000000000000000u64),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: Some(vec!["test3".to_string()]),
            api_key: None,
        };

        let result = db_service.store_builder_info(&public_key, &builder_info).await;
        assert!(result.is_ok());

        let result = db_service.get_all_builder_infos().await.unwrap();
        assert!(result.len() == 1);
        assert!(result[0].pub_key == public_key);
    }

    #[tokio::test]
    async fn test_demotion() {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let key = BlsSecretKey::random();
        let public_key = key.public_key().serialize().into();

        let builder_info = helix_common::BuilderInfo {
            collateral: Default::default(),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };

        let result = db_service.store_builder_info(&public_key, &builder_info).await;
        assert!(result.is_ok());

        let result =
            db_service.db_demote_builder(0, &public_key, &Default::default(), "".to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_store_block_submission() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let pubkey = BlsPublicKey::deserialize(alloy_primitives::hex!("8592669BC0ACF28BC25D42699CEFA6101D7B10443232FE148420FF0FCDBF8CD240F5EBB94BC904CB6BEFFB61A1F8D36A").as_ref()).unwrap();

        let bid_trace =
            BidTrace { proposer_pubkey: pubkey.serialize().into(), ..BidTrace::test_random() };

        let signed_bid_submission = SignedBidSubmissionElectra {
            message: bid_trace.clone(),
            execution_payload: ExecutionPayload::test_random().into(),
            blobs_bundle: BlobsBundle::default().into(),
            signature: BlsSignatureBytes::random(),
            execution_requests: Default::default(),
        };

        let submission_trace = SubmissionTrace { receive: utcnow_ns(), ..Default::default() };

        db_service
            .store_block_submission(
                signed_bid_submission.into(),
                submission_trace,
                OptimisticVersion::NotOptimistic,
            )
            .await?;
        Ok(())
    }

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

    #[tokio::test]
    async fn test_save_delivered_payloads() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let mut execution_payload = ExecutionPayload::test_random();

        // execution_payload
        //     .transactions_mut()
        //     .push(ethereum_consensus::capella::Transaction::default());

        // execution_payload
        //     .transactions_mut()
        //     .push(ethereum_consensus::capella::Transaction::default());
        execution_payload
            .withdrawals
            .push(Withdrawal {
                index: 0,
                validator_index: 0,
                amount: 0,
                address: Default::default(),
            })
            .unwrap();

        let latency_trace = GetPayloadTrace::default();

        let payload_and_blobs =
            PayloadAndBlobs { execution_payload, blobs_bundle: Default::default() };

        db_service
            .save_delivered_payload(
                BlsPublicKeyBytes::random(),
                Arc::new(payload_and_blobs),
                &latency_trace,
                None,
            )
            .await?;
        Ok(())
    }

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

    #[tokio::test]
    async fn test_get_header() -> Result<(), Box<dyn std::error::Error>> {
        run_setup().await;

        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_get_header_call(
                1,
                Default::default(),
                reg.message.pubkey,
                Default::default(),
                Default::default(),
                false,
                None,
            )
            .await?;

        Ok(())
    }

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
            let insert_index = rng.random_range(0..=existing_vec.len()); // Generate a random index
            existing_vec.insert(insert_index, value); // Insert the new value at the random index
        }
    }
}
