#[cfg(test)]
mod tests {
    use crate::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
    use ethereum_consensus::{
        builder::{SignedValidatorRegistration, ValidatorRegistration},
        clock::get_current_unix_time_in_nanos,
        crypto::{PublicKey, SecretKey},
        primitives::U256,
    };
    use helix_common::{
        bellatrix::{ByteList, ByteVector, List},
        bid_submission::{
            v2::header_submission::{
                HeaderSubmissionCapella, SignedHeaderSubmission, SignedHeaderSubmissionCapella,
            },
            BidTrace, SignedBidSubmission,
        },
        versioned_payload::PayloadAndBlobs,
        Filtering, GetPayloadTrace, HeaderSubmissionTrace, SubmissionTrace, ValidatorSummary,
    };
    use rand::{seq::SliceRandom, thread_rng, Rng};
    use std::{
        default::Default,
        ops::DerefMut,
        str::FromStr,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use tokio::time::sleep;

    use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod};
    use ethereum_consensus::phase0::Validator;
    use helix_common::{
        api::{
            builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
        },
        simulator::BlockSimError,
        validator_preferences::ValidatorPreferences,
    };
    use tokio_postgres::NoTls;

    use crate::postgres::postgres_db_init::run_migrations_async;

    /// These tests depend on a local instance of postgres running on port 5433
    /// e.g. to start a local postgres instance in docker:
    /// docker run -d --name postgres -e POSTGRES_PASSWORD=password -p 5433:5432
    /// timescale/timescaledb-ha:pg16 https://docs.timescale.com/self-hosted/latest/install/installation-docker/

    fn test_config() -> Config {
        let mut cfg = Config::new();
        cfg.host = Some("localhost".to_string());
        cfg.port = Some(5433);
        cfg.dbname = Some("postgres".to_string());
        cfg.user = Some("postgres".to_string());
        cfg.password = Some("password".to_string());
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
        cfg
    }

    fn setup_test_pool() -> Result<Pool, Box<dyn std::error::Error>> {
        Ok(test_config().create_pool(None, NoTls)?)
    }

    async fn setup_test_conn() -> Result<deadpool_postgres::Client, Box<dyn std::error::Error>> {
        let pool = setup_test_pool()?;
        let client = pool.get().await?;

        // ping the database to make sure we're connected
        let resp = client.query_one("SELECT 1", &[]).await?;
        assert!(resp.get::<_, i32>(0) == 1);

        Ok(client)
    }

    #[tokio::test]
    async fn test_run_migrations_async() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let mut conn = setup_test_conn().await?;
        let client = conn.deref_mut().deref_mut();
        match run_migrations_async(client).await {
            Ok(report) => {
                println!("Applied migrations: {}", report.applied_migrations().len());
                println!("Migrations: {:?}", report);
                Ok(())
            }
            Err(e) => {
                println!("Error applying migrations: {}", e);
                Err(e)
            }
        }
    }

    fn get_randomized_signed_validator_registration() -> ValidatorRegistrationInfo {
        let mut rng = rand::thread_rng();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let gas_limit = 0;
        let key = SecretKey::random(&mut rng).unwrap();
        let signature = key.sign("message".as_bytes());
        let public_key = key.public_key();
        ValidatorRegistrationInfo {
            registration: SignedValidatorRegistration {
                message: ValidatorRegistration {
                    fee_recipient: Default::default(),
                    timestamp,
                    gas_limit,
                    public_key: public_key.clone(),
                },
                signature,
            },
            preferences: ValidatorPreferences {
                filtering: Filtering::Global,
                trusted_builders: Some(vec!["test".to_string(), "test2".to_string()]),
                header_delay: true,
                gossip_blobs: true,
            },
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registration() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        db_service.start_registration_processor().await;

        let registration = get_randomized_signed_validator_registration();

        db_service
            .save_validator_registration(registration.clone(), Some("test".to_string()))
            .await
            .unwrap();
        sleep(Duration::from_secs(5)).await;

        let result = db_service
            .get_validator_registration(registration.registration.message.public_key)
            .await
            .unwrap();
        assert_eq!(
            result.registration_info.registration.signature,
            registration.registration.signature
        );
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registrations() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        db_service.start_registration_processor().await;

        const NUM_REGISTRATIONS: usize = 2;

        let registrations = (0..NUM_REGISTRATIONS)
            .map(|_| get_randomized_signed_validator_registration())
            .collect::<Vec<_>>();

        db_service
            .save_validator_registrations(registrations.clone(), Some("test".to_string()))
            .await
            .unwrap();
        sleep(Duration::from_secs(5)).await;

        for registration in registrations {
            let result = db_service
                .get_validator_registration(registration.registration.message.public_key)
                .await
                .unwrap();
            assert_eq!(
                result.registration_info.registration.signature,
                registration.registration.signature
            );
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registrations_for_pub_keys() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        db_service.start_registration_processor().await;

        const N_REGISTRATIONS: usize = 2;

        let registrations = (0..N_REGISTRATIONS)
            .map(|_| get_randomized_signed_validator_registration())
            .collect::<Vec<_>>();

        db_service
            .save_validator_registrations(registrations.clone(), Some("test".to_string()))
            .await
            .unwrap();

        sleep(Duration::from_secs(5)).await;

        let result = db_service
            .get_validator_registrations_for_pub_keys(
                registrations
                    .iter()
                    .map(|r| r.registration.message.public_key.clone())
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();

        for registration in registrations {
            let result = result
                .iter()
                .find(|r| {
                    r.registration_info.registration.message.public_key ==
                        registration.registration.message.public_key
                })
                .unwrap();
            assert_eq!(
                result.registration_info.registration.signature,
                registration.registration.signature
            );
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registration_timestamp() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        db_service.start_registration_processor().await;

        let registration = get_randomized_signed_validator_registration();
        db_service
            .save_validator_registration(registration.clone(), Some("test".to_string()))
            .await
            .unwrap();

        sleep(Duration::from_secs(5)).await;

        let result = db_service
            .get_validator_registration_timestamp(registration.registration.message.public_key)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_and_get_proposer_duties() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let mut proposer_duties = Vec::new();
        for i in 0..10 {
            let registration = get_randomized_signed_validator_registration();
            db_service
                .save_validator_registration(registration.clone(), Some("test".to_string()))
                .await
                .unwrap();

            proposer_duties.push(BuilderGetValidatorsResponseEntry {
                slot: i,
                validator_index: i as usize,
                entry: registration.clone(),
            });
        }

        let result = db_service.set_proposer_duties(proposer_duties).await;
        assert!(result.is_ok());

        let result = db_service.get_proposer_duties().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_and_get_known_validators() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut validator_summaries = Vec::new();

        for i in 0..100 {
            let mut rng = rand::thread_rng();
            let key = SecretKey::random(&mut rng).unwrap();
            let public_key = key.public_key();

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    public_key: public_key.clone(),
                    withdrawal_credentials: Default::default(),
                    effective_balance: 0,
                    slashed: false,
                    activation_eligibility_epoch: 0,
                    activation_epoch: 0,
                    exit_epoch: 0,
                    withdrawable_epoch: 0,
                },
            };

            validator_summaries.push(validator_summary);
        }

        let mut validator_summaries_clone = validator_summaries.clone();

        let result = db_service.set_known_validators(validator_summaries).await;
        assert!(result.is_ok());

        let mut new_validator_summaries = Vec::new();

        for i in 0..10 {
            let mut rng = rand::thread_rng();
            let key = SecretKey::random(&mut rng).unwrap();
            let public_key = key.public_key();

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    public_key: public_key.clone(),
                    withdrawal_credentials: Default::default(),
                    effective_balance: 0,
                    slashed: false,
                    activation_eligibility_epoch: 0,
                    activation_epoch: 0,
                    exit_epoch: 0,
                    withdrawable_epoch: 0,
                },
            };

            new_validator_summaries.push(validator_summary);
        }

        let removed = remove_random_items::<ValidatorSummary>(&mut validator_summaries_clone, 5);

        randomly_insert_values::<ValidatorSummary>(
            &mut validator_summaries_clone,
            new_validator_summaries,
        );

        let final_list = validator_summaries_clone.clone();

        let result = db_service.set_known_validators(validator_summaries_clone).await;
        assert!(result.is_ok());

        // Check that the removed validators are no longer known
        for removed_validator in removed {
            let result = db_service
                .check_known_validators(vec![removed_validator.validator.public_key])
                .await;
            assert!(result.is_ok());
            assert!(result.unwrap().is_empty());
        }

        // Check that all validators in the final list are known
        for new_validator in final_list {
            let result =
                db_service.check_known_validators(vec![new_validator.validator.public_key]).await;
            assert!(result.is_ok());
            assert!(!result.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn test_save_large_batch() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut rng = rand::thread_rng();
        let mut validator_summaries = Vec::new();

        for i in 0..200_000 {
            let key = SecretKey::random(&mut rng).unwrap();
            let public_key = key.public_key();

            let validator_summary = helix_common::ValidatorSummary {
                index: i,
                balance: 0,
                status: helix_common::ValidatorStatus::Active,
                validator: Validator {
                    public_key: public_key.clone(),
                    withdrawal_credentials: Default::default(),
                    effective_balance: 0,
                    slashed: false,
                    activation_eligibility_epoch: 0,
                    activation_epoch: 0,
                    exit_epoch: 0,
                    withdrawable_epoch: 0,
                },
            };

            validator_summaries.push(validator_summary);
        }

        let result = db_service.set_known_validators(validator_summaries).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_and_get_builder_info() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let public_key = PublicKey::try_from(hex::decode("8C266FD5CB50B5D9431DAA69C4BE17BC9A79A85D172112DA09E0AC3E2D0DCF785021D49B6DF57827D6BC61EBA086A507").unwrap().as_ref()).unwrap();
        let builder_info = helix_common::BuilderInfo {
            collateral: U256::from_str("1000000000000000000000000000").unwrap(),
            is_optimistic: false,
            builder_id: None,
        };

        let result = db_service.store_builder_info(&public_key, &builder_info).await;
        assert!(result.is_ok());

        let result = db_service.db_get_builder_info(&public_key).await;
        assert!(result.is_ok());

        let result = db_service.get_all_builder_infos().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_demotion() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let mut rng = rand::thread_rng();
        let key = SecretKey::random(&mut rng).unwrap();
        let public_key = key.public_key();

        let builder_info = helix_common::BuilderInfo {
            collateral: Default::default(),
            is_optimistic: false,
            builder_id: None,
        };

        let result = db_service.store_builder_info(&public_key, &builder_info).await;
        assert!(result.is_ok());

        let result =
            db_service.db_demote_builder(&public_key, &Default::default(), "".to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_simulation_result() {
        let _ = env_logger::builder().is_test(true).try_init();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let block_hash = Default::default();
        let block_sim_result = Err(BlockSimError::Timeout);

        let result = db_service.save_simulation_result(block_hash, block_sim_result).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_store_block_submission() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let mut rng = rand::thread_rng(); // Get a random number generator
        let random_bytes: [u8; 32] = rng.gen();

        let bid_trace = BidTrace {
            slot: 1235,
            parent_hash: Default::default(),
            block_hash: ByteVector::<32>::try_from(random_bytes.as_slice()).unwrap(),
            builder_public_key: Default::default(),
            proposer_public_key:  PublicKey::try_from(hex::decode("8592669BC0ACF28BC25D42699CEFA6101D7B10443232FE148420FF0FCDBF8CD240F5EBB94BC904CB6BEFFB61A1F8D36A").unwrap().as_ref()).unwrap(),
            proposer_fee_recipient: Default::default(),
            gas_limit: 0,
            gas_used: 0,
            value: U256::from(1234),
        };
        let mut signed_bid_submission = SignedBidSubmission::default();
        match &mut signed_bid_submission {
            SignedBidSubmission::Deneb(submission) => {
                submission.message = bid_trace.clone();
            }
            SignedBidSubmission::Capella(submission) => {
                submission.message = bid_trace.clone();
            }
        }

        let submission_trace = SubmissionTrace {
            receive: get_current_unix_time_in_nanos() as u64,
            ..Default::default()
        };

        db_service
            .store_block_submission(Arc::new(signed_bid_submission), Arc::new(submission_trace), 0)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_get_bids() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
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
        let bids = db_service.get_bids(&filter).await?;
        println!("Bids: {:?}", bids);
        Ok(())
    }

    #[tokio::test]
    async fn test_save_delivered_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let extra_data = [0u8; 32];
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;
        let mut execution_payload = ethereum_consensus::types::ExecutionPayload::Capella(
            ethereum_consensus::capella::ExecutionPayload {
                parent_hash: ByteVector::default(),
                fee_recipient: ByteVector::default(),
                state_root: ByteVector::default(),
                receipts_root: ByteVector::default(),
                logs_bloom: ByteVector::default(),
                prev_randao: ByteVector::default(),
                block_number: 1234,
                gas_limit: 0,
                gas_used: 0,
                timestamp: 0,
                extra_data: ByteList::try_from(extra_data.as_slice()).unwrap(),
                base_fee_per_gas: U256::from(1234),
                block_hash: ByteVector::try_from(
                    hex::decode("6AD0CC0183284A1F2CEBB5188DC68F49EC6D522D9E99706DA097EF2BD8148D88")
                        .unwrap()
                        .as_slice(),
                )
                .unwrap(),
                transactions: List::default(),
                withdrawals: Default::default(),
            },
        );

        // execution_payload
        //     .transactions_mut()
        //     .push(ethereum_consensus::capella::Transaction::default());

        // execution_payload
        //     .transactions_mut()
        //     .push(ethereum_consensus::capella::Transaction::default());
        execution_payload.withdrawals_mut().unwrap().push(
            ethereum_consensus::capella::Withdrawal {
                index: 0,
                validator_index: 0,
                amount: 0,
                address: Default::default(),
            },
        );

        let bid_trace =  BidTrace {
            slot: 1235,
            block_hash: ByteVector::try_from(
            hex::decode("6AD0CC0183284A1F2CEBB5188DC68F49EC6D522D9E99706DA097EF2BD8148D88")
                .unwrap()
                .as_slice(),
        )
        .unwrap(),
            proposer_public_key: PublicKey::try_from(
                hex::decode("8592669BC0ACF28BC25D42699CEFA6101D7B10443232FE148420FF0FCDBF8CD240F5EBB94BC904CB6BEFFB61A1F8D36A").unwrap().as_ref()).unwrap(),
            ..Default::default() };
        let latency_trace = GetPayloadTrace::default();

        let payload_and_blobs =
            PayloadAndBlobs { execution_payload: execution_payload.clone(), blobs_bundle: None };

        db_service
            .save_delivered_payload(&bid_trace, Arc::new(payload_and_blobs), &latency_trace)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_get_delivered_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
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
        println!("delivered payloads {:?}", delivered_payloads);
        Ok(())
    }

    #[tokio::test]
    async fn test_late_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_too_late_get_payload(1, &reg.message.public_key, &Default::default(), 0, 0)
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_get_header() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_get_header_call(
                1,
                Default::default(),
                reg.message.public_key,
                Default::default(),
                Default::default(),
                None,
            )
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_failed_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let _reg = get_randomized_signed_validator_registration().registration;

        db_service
            .save_failed_get_payload(1, Default::default(), "error".to_string(), Default::default())
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_store_header_submission() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let signed_bid_submission =
            SignedHeaderSubmission::Capella(SignedHeaderSubmissionCapella {
                message: HeaderSubmissionCapella {
                    bid_trace: BidTrace {
                        slot: 1234,
                        parent_hash: Default::default(),
                        block_hash: Default::default(),
                        builder_public_key: Default::default(),
                        proposer_public_key: Default::default(),
                        proposer_fee_recipient: Default::default(),
                        gas_limit: 0,
                        gas_used: 0,
                        value: U256::from(1234),
                    },
                    execution_payload_header: Default::default(),
                },
                signature: Default::default(),
            });

        db_service
            .store_header_submission(
                Arc::new(signed_bid_submission),
                Arc::new(HeaderSubmissionTrace::default()),
            )
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_gossiped_header() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;
        db_service.save_gossiped_header_trace(Default::default(), Default::default()).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_gossiped_payload() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;
        db_service.save_gossiped_payload_trace(Default::default(), Default::default()).await?;

        Ok(())
    }

    fn remove_random_items<T>(vec: &mut Vec<T>, count: usize) -> Vec<T> {
        let mut rng = thread_rng();

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
        let mut rng = rand::thread_rng();

        for value in new_values {
            let insert_index = rng.gen_range(0..=existing_vec.len()); // Generate a random index
            existing_vec.insert(insert_index, value); // Insert the new value at the random index
        }
    }
}
