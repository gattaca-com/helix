#[cfg(test)]
mod tests {
    use std::{default::Default, str::FromStr};
use crate::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
    use ethereum_consensus::{
        builder::{SignedValidatorRegistration, ValidatorRegistration},
        crypto::SecretKey,
        primitives::U256,
    };
    use std::{
        ops::DerefMut,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };
    use helix_common::{
        bellatrix::{ByteVector, ByteList, List}, bid_submission::{BidTrace, SignedBidSubmission, v2::header_submission::SignedHeaderSubmission}, versioned_payload::PayloadAndBlobs, GetPayloadTrace, HeaderSubmissionTrace
    };

    use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod};
    use ethereum_consensus::phase0::Validator;
    use helix_common::{
        api::builder_api::BuilderGetValidatorsResponseEntry, simulator::BlockSimError,
    };
    use tokio_postgres::NoTls;
    use helix_common::api::proposer_api::ValidatorRegistrationInfo;
    use helix_common::validator_preferences::ValidatorPreferences;

    use crate::postgres::postgres_db_init::run_migrations_async;

    /// These tests depend on a local instance of postgres running on port 5433
    /// e.g. to start a local postgres instance in docker:
    /// docker run -d --name postgres -e POSTGRES_PASSWORD=password -p 5433:5432 timescale/timescaledb-ha:pg16
    /// https://docs.timescale.com/self-hosted/latest/install/installation-docker/

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
                censoring: false,
                trusted_builders: Some(vec!["test".to_string(), "test2".to_string()]),
            },
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registration() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let registration = get_randomized_signed_validator_registration();

        db_service.save_validator_registration(registration.clone()).await.unwrap();

        let result =
            db_service.get_validator_registration(registration.registration.message.public_key).await.unwrap();
        assert_eq!(result.registration_info.registration.signature, registration.registration.signature);
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registrations() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        const NUM_REGISTRATIONS: usize = 2;

        let registrations = (0..NUM_REGISTRATIONS)
            .map(|_| get_randomized_signed_validator_registration())
            .collect::<Vec<_>>();

        db_service.save_validator_registrations(registrations.clone()).await.unwrap();

        for registration in registrations {
            let result = db_service
                .get_validator_registration(registration.registration.message.public_key)
                .await
                .unwrap();
            assert_eq!(result.registration_info.registration.signature, registration.registration.signature);
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registrations_for_pub_keys() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        const N_REGISTRATIONS: usize = 2;

        let registrations = (0..N_REGISTRATIONS)
            .map(|_| get_randomized_signed_validator_registration())
            .collect::<Vec<_>>();

        db_service.save_validator_registrations(registrations.clone()).await.unwrap();
        let result = db_service
            .get_validator_registrations_for_pub_keys(
                registrations.iter().map(|r| r.registration.message.public_key.clone()).collect::<Vec<_>>(),
            )
            .await
            .unwrap();

        for registration in registrations {
            let result = result
                .iter()
                .find(|r| r.registration_info.registration.message.public_key == registration.registration.message.public_key)
                .unwrap();
            assert_eq!(result.registration_info.registration.signature, registration.registration.signature);
        }
    }

    #[tokio::test]
    async fn test_save_and_get_validator_registration_timestamp() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let registration = get_randomized_signed_validator_registration();
        db_service.save_validator_registration(registration.clone()).await.unwrap();

        let result =
            db_service.get_validator_registration_timestamp(registration.registration.message.public_key).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_and_get_proposer_duties() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let registration = get_randomized_signed_validator_registration();
        db_service.save_validator_registration(registration.clone()).await.unwrap();

        let proposer_duties = vec![BuilderGetValidatorsResponseEntry {
            slot: 0,
            validator_index: 0,
            entry: registration.clone(),
        }];

        let result = db_service.set_proposer_duties(proposer_duties).await;
        assert!(result.is_ok());

        let result = db_service.get_proposer_duties().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_and_get_known_validators() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut validator_summaries = Vec::new();

        let mut rng = rand::thread_rng();
        let key1 = SecretKey::random(&mut rng).unwrap();
        let public_key1 = key1.public_key();
        let key2 = SecretKey::random(&mut rng).unwrap();
        let public_key2 = key2.public_key();

        let validator_summary1 = helix_common::ValidatorSummary {
            index: 0,
            balance: 0,
            status: helix_common::ValidatorStatus::Active,
            validator: Validator {
                public_key: public_key1.clone(),
                withdrawal_credentials: Default::default(),
                effective_balance: 0,
                slashed: false,
                activation_eligibility_epoch: 0,
                activation_epoch: 0,
                exit_epoch: 0,
                withdrawable_epoch: 0,
            },
        };

        validator_summaries.push(validator_summary1);

        let validator_summary2 = helix_common::ValidatorSummary {
            index: 1,
            balance: 0,
            status: helix_common::ValidatorStatus::Active,
            validator: Validator {
                public_key: public_key2.clone(),
                withdrawal_credentials: Default::default(),
                effective_balance: 0,
                slashed: false,
                activation_eligibility_epoch: 0,
                activation_epoch: 0,
                exit_epoch: 0,
                withdrawable_epoch: 0,
            },
        };

        validator_summaries.push(validator_summary2);


        let result = db_service.set_known_validators(validator_summaries).await;
        assert!(result.is_ok());

        let result = db_service.check_known_validators(vec![public_key1, public_key2]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_large_batch() {
        env_logger::builder().is_test(true).try_init().unwrap();
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
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();

        let mut rng = rand::thread_rng();
        let key = SecretKey::random(&mut rng).unwrap();
        let public_key = key.public_key();
        let builder_info =
            helix_common::BuilderInfo { collateral: U256::from_str("1000000000000000000000000000").unwrap(), is_optimistic: false, builder_id: None };

        let result = db_service.store_builder_info(&public_key, builder_info).await;
        assert!(result.is_ok());

        let result = db_service.db_get_builder_info(&public_key).await;
        assert!(result.is_ok());

        let result = db_service.get_all_builder_infos().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_demotion() {
        env_logger::builder().is_test(true).try_init().unwrap();
        let db_service = PostgresDatabaseService::new(&test_config(), 0).unwrap();
        let mut rng = rand::thread_rng();
        let key = SecretKey::random(&mut rng).unwrap();
        let public_key = key.public_key();

        let builder_info =
            helix_common::BuilderInfo { collateral: Default::default(), is_optimistic: false, builder_id: None };

        let result = db_service.store_builder_info(&public_key, builder_info).await;
        assert!(result.is_ok());

        let result = db_service.db_demote_builder(&public_key, &Default::default(), "".to_string()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_save_simulation_result() {
        env_logger::builder().is_test(true).try_init().unwrap();
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

        let bid_trace = BidTrace {
            slot: 1234,
            parent_hash: Default::default(),
            block_hash: Default::default(),
            builder_public_key: Default::default(),
            proposer_public_key: Default::default(),
            proposer_fee_recipient: Default::default(),
            gas_limit: 0,
            gas_used: 0,
            value: U256::from(1234),
        };
        let mut signed_bid_submission = SignedBidSubmission::default();
        match &mut signed_bid_submission {
            SignedBidSubmission::Deneb(submission) => {
                submission.message = bid_trace.clone();
            },
            SignedBidSubmission::Capella(submission) => {
                submission.message = bid_trace.clone();
            },
        }

        db_service.store_block_submission(Arc::new(signed_bid_submission), Arc::new(Default::default()), 0).await?;
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
        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;
        let mut execution_payload =
            ethereum_consensus::types::ExecutionPayload::Capella(
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
                    block_hash: ByteVector::default(),
                    transactions: List::default(),
                    withdrawals: Default::default(),
                },
            );

        execution_payload.transactions_mut().push(ethereum_consensus::capella::Transaction::default());
        execution_payload.withdrawals_mut().unwrap().push(ethereum_consensus::capella::Withdrawal {
            index: 0,
            validator_index: 0,
            amount: 0,
            address: Default::default(),
        });

        let mut bid_trace = BidTrace::default();
        bid_trace.slot = 1234;
        let latency_trace = GetPayloadTrace::default();

        let payload_and_blobs = PayloadAndBlobs {
            execution_payload: execution_payload.clone(),
            blobs_bundle: None,
        };

        db_service.save_delivered_payload(&bid_trace, Arc::new(payload_and_blobs), &latency_trace).await?;
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
        let delivered_payloads = db_service.get_delivered_payloads(&filter).await?;
        println!("delivered payloads {:?}", delivered_payloads);
        Ok(())
    }

    #[tokio::test]
    async fn test_late_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 0)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service.save_too_late_get_payload(1, &reg.message.public_key, &Default::default(),0,0).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_get_header() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let reg = get_randomized_signed_validator_registration().registration;

        db_service.save_get_header_call(1, Default::default(), reg.message.public_key, Default::default(), Default::default()).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_failed_payloads() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let _reg = get_randomized_signed_validator_registration().registration;

        db_service.save_failed_get_payload(Default::default(), "error".to_string(), Default::default()).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_store_header_submission() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;

        let mut signed_bid_submission = SignedHeaderSubmission::default();
        *signed_bid_submission.bid_trace_mut() = BidTrace {
            slot: 1234,
            parent_hash: Default::default(),
            block_hash: Default::default(),
            builder_public_key: Default::default(),
            proposer_public_key: Default::default(),
            proposer_fee_recipient: Default::default(),
            gas_limit: 0,
            gas_used: 0,
            value: U256::from(1234),
        };
        db_service.store_header_submission(Arc::new(signed_bid_submission), Arc::new(HeaderSubmissionTrace::default())).await?;
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

    #[tokio::test]
    async fn test_save_pending_block() -> Result<(), Box<dyn std::error::Error>> {
        env_logger::builder().is_test(true).try_init()?;
        let db_service = PostgresDatabaseService::new(&test_config(), 1)?;
        db_service.save_pending_block(&Default::default(), &Default::default(), 359023, SystemTime::now()).await?;

        Ok(())
    }
}
