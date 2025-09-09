use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256, U256};
use async_trait::async_trait;
use helix_common::{
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        data_api::BidFilters,
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{v2::header_submission::SignedHeaderSubmission, OptimisticVersion},
    simulator::BlockSimError,
    BuilderInfo, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace, HeaderSubmissionTrace,
    ProposerInfo, SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorPreferences,
    ValidatorSummary,
};
use helix_types::{
    BlsPublicKeyBytes, BlsSignatureBytes, PayloadAndBlobs, SignedBidSubmission,
    SignedValidatorRegistration, TestRandomSeed, ValidatorRegistrationData,
};

use crate::{
    error::DatabaseError, BidSubmissionDocument, BuilderInfoDocument, DatabaseService,
    DeliveredPayloadDocument,
};

#[derive(Default, Clone)]
pub struct MockDatabaseService {
    pub known_validators: Arc<Mutex<Vec<ValidatorSummary>>>,
    pub proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>>,
}

impl MockDatabaseService {
    pub fn new(
        known_validators: Arc<Mutex<Vec<ValidatorSummary>>>,
        proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>>,
    ) -> Self {
        Self { known_validators, proposer_duties }
    }
}

#[async_trait]
impl DatabaseService for MockDatabaseService {
    async fn save_validator_registrations(
        &self,
        _entries: Vec<ValidatorRegistrationInfo>,
        _pool_name: Option<String>,
        _user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
    async fn is_registration_update_required(
        &self,
        _registration: &SignedValidatorRegistration,
    ) -> Result<bool, DatabaseError> {
        Ok(true)
    }
    async fn get_validator_registration(
        &self,
        pubkey: &BlsPublicKeyBytes,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError> {
        let registration = SignedValidatorRegistration {
            message: ValidatorRegistrationData {
                fee_recipient: Address::test_random(),
                gas_limit: 1000000,
                timestamp: 1000000,
                pubkey: *pubkey,
            },
            signature: BlsSignatureBytes::random(),
        };

        Ok(SignedValidatorRegistrationEntry {
            registration_info: ValidatorRegistrationInfo {
                registration,
                preferences: ValidatorPreferences::default(),
            },
            inserted_at: 0,
            pool_name: None,
            user_agent: None,
        })
    }
    async fn get_validator_registrations_for_pub_keys(
        &self,
        pubkeys: &[&BlsPublicKeyBytes],
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let mut entries = vec![];
        for &pubkey in pubkeys {
            entries.push(self.get_validator_registration(pubkey).await?);
        }
        Ok(entries)
    }

    async fn set_proposer_duties(
        &self,
        proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), DatabaseError> {
        println!("received proposer duties: {proposer_duties:?}");
        self.proposer_duties.lock().unwrap().clear();
        self.proposer_duties.lock().unwrap().extend(proposer_duties);

        Ok(())
    }
    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, DatabaseError> {
        Ok(vec![])
    }

    async fn set_known_validators(
        &self,
        known_validators: Vec<ValidatorSummary>,
    ) -> Result<(), DatabaseError> {
        println!("received known validators: {known_validators:?}");
        self.known_validators.lock().unwrap().clear();
        self.known_validators.lock().unwrap().extend(known_validators);

        Ok(())
    }

    async fn check_known_validators(
        &self,
        public_keys: Vec<BlsPublicKeyBytes>,
    ) -> Result<HashSet<BlsPublicKeyBytes>, DatabaseError> {
        Ok(public_keys.into_iter().collect())
    }

    async fn save_too_late_get_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKeyBytes,
        _payload_hash: &B256,
        _message_received: u64,
        _payload_fetched: u64,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_delivered_payload(
        &self,
        _proposer_pub_key: BlsPublicKeyBytes,
        _payload: Arc<PayloadAndBlobs>,
        _latency_trace: &GetPayloadTrace,
        _user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_block_submission(
        &self,
        _submission: SignedBidSubmission,
        _trace: SubmissionTrace,
        _optimistic_version: OptimisticVersion,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKeyBytes,
        _builder_info: &BuilderInfo,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_builders_info(
        &self,
        _builders: &[BuilderInfoDocument],
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError> {
        Ok(vec![])
    }

    async fn check_builder_api_key(&self, api_key: &str) -> Result<bool, DatabaseError> {
        if api_key == "valid" {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn db_demote_builder(
        &self,
        _slot: u64,
        _builder_pub_key: &BlsPublicKeyBytes,
        _block_hash: &B256,
        _reason: String,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_simulation_result(
        &self,
        _block_hash: B256,
        _block_sim_result: Result<(), BlockSimError>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn get_bids(
        &self,
        _filters: &BidFilters,
        _validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError> {
        let mut bid = BidSubmissionDocument::test_random();
        bid.bid_trace.value = U256::from(1000);
        Ok(vec![bid])
    }

    async fn get_delivered_payloads(
        &self,
        _filters: &BidFilters,
        _validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        todo!()
        // let doc = DeliveredPayloadDocument {
        //     bid_trace: BidTrace::random_for_test(),
        //     block_number: 0,
        //     num_txs: 0,
        // };

        // Ok(vec![doc])
    }

    async fn save_get_header_call(
        &self,
        _slot: u64,
        _parent_hash: B256,
        _public_key: BlsPublicKeyBytes,
        _best_block_hash: B256,
        _trace: GetHeaderTrace,

        _mev_boost: bool,
        _user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_failed_get_payload(
        &self,
        _slot: u64,
        _block_hash: B256,
        _error: String,
        _trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
    async fn store_header_submission(
        &self,
        _submission: Arc<SignedHeaderSubmission>,
        _trace: HeaderSubmissionTrace,
        _tx_count: u32,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_gossiped_payload_trace(
        &self,
        _block_hash: B256,
        _trace: GossipedPayloadTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError> {
        Ok(vec![])
    }

    async fn get_validator_pool_name(
        &self,
        api_key: &str,
    ) -> Result<Option<String>, DatabaseError> {
        if api_key == "valid" {
            Ok(Some("test_pool".to_string()))
        } else {
            Ok(None)
        }
    }

    async fn get_validator_registrations(
        &self,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        Ok(vec![])
    }

    async fn save_inclusion_list(
        &self,
        _: &InclusionListWithMetadata,
        _: u64,
        _: &B256,
        _: &BlsPublicKeyBytes,
    ) -> Result<(), Vec<DatabaseError>> {
        Ok(())
    }
}
