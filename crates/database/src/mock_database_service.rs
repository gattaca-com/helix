use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ethereum_consensus::{
    primitives::{BlsPublicKey, Hash32},
    ssz::prelude::*,
};
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, data_api::BidFilters,
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission, BidTrace, SignedBidSubmission,
    },
    deneb::SignedValidatorRegistration,
    simulator::BlockSimError,
    versioned_payload::PayloadAndBlobs,
    BuilderInfo, GetHeaderTrace, GetPayloadTrace, GossipedHeaderTrace, GossipedPayloadTrace,
    HeaderSubmissionTrace, ProposerInfo, SignedValidatorRegistrationEntry, SubmissionTrace,
    ValidatorPreferences, ValidatorSummary,
};

use crate::{
    error::DatabaseError, BidSubmissionDocument, BuilderInfoDocument, DatabaseService,
    DeliveredPayloadDocument,
};

#[derive(Default, Clone)]
pub struct MockDatabaseService {
    known_validators: Arc<Mutex<Vec<ValidatorSummary>>>,
    proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>>,
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
    async fn save_validator_registration(
        &self,
        _entry: ValidatorRegistrationInfo,
        _pool_name: Option<String>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
    async fn save_validator_registrations(
        &self,
        _entries: Vec<ValidatorRegistrationInfo>,
        _pool_name: Option<String>,
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
        _pub_key: BlsPublicKey,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError> {
        Ok(SignedValidatorRegistrationEntry::default())
    }
    async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: Vec<BlsPublicKey>,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let mut entries = vec![];
        for _pub_key in pub_keys {
            entries.push(SignedValidatorRegistrationEntry::default());
        }
        Ok(entries)
    }

    async fn get_validator_registration_timestamp(
        &self,
        _pub_key: BlsPublicKey,
    ) -> Result<u64, DatabaseError> {
        Ok(0)
    }

    async fn set_proposer_duties(
        &self,
        proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), DatabaseError> {
        println!("received proposer duties: {:?}", proposer_duties);
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
        println!("received known validators: {:?}", known_validators);
        self.known_validators.lock().unwrap().clear();
        self.known_validators.lock().unwrap().extend(known_validators);

        Ok(())
    }

    async fn check_known_validators(
        &self,
        public_keys: Vec<BlsPublicKey>,
    ) -> Result<HashSet<BlsPublicKey>, DatabaseError> {
        Ok(public_keys.into_iter().collect())
    }

    async fn save_too_late_get_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _payload_hash: &Hash32,
        _message_received: u64,
        _payload_fetched: u64,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_delivered_payload(
        &self,
        _bid_trace: &BidTrace,
        _payload: Arc<PayloadAndBlobs>,
        _latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_block_submission(
        &self,
        _submission: Arc<SignedBidSubmission>,
        _trace: Arc<SubmissionTrace>,
        _optimistic_version: i16,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKey,
        _builder_info: &BuilderInfo,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_builders_info(
        &self,
        _builders: &Vec<BuilderInfoDocument>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn db_get_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, DatabaseError> {
        Ok(BuilderInfo::default())
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
        _builder_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
        _reason: String,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_simulation_result(
        &self,
        _block_hash: ByteVector<32>,
        _block_sim_result: Result<(), BlockSimError>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn get_bids(
        &self,
        _filters: &BidFilters,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError> {
        let mut bid = BidSubmissionDocument::default();
        bid.bid_trace.value = U256::from(1000);
        Ok(vec![bid])
    }

    async fn get_delivered_payloads(
        &self,
        _filters: &BidFilters,
        _validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let doc =
            DeliveredPayloadDocument { bid_trace: Default::default(), block_number: 0, num_txs: 0 };

        Ok(vec![doc])
    }

    async fn save_get_header_call(
        &self,
        _slot: u64,
        _parent_hash: ByteVector<32>,
        _public_key: BlsPublicKey,
        _best_block_hash: ByteVector<32>,
        _trace: GetHeaderTrace,
        _user_agent: Option<String>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_failed_get_payload(
        &self,
        _slot: u64,
        _block_hash: ByteVector<32>,
        _error: String,
        _trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
    async fn store_header_submission(
        &self,
        _submission: Arc<SignedHeaderSubmission>,
        _trace: Arc<HeaderSubmissionTrace>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_gossiped_header_trace(
        &self,
        _block_hash: ByteVector<32>,
        _trace: Arc<GossipedHeaderTrace>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_gossiped_payload_trace(
        &self,
        _block_hash: ByteVector<32>,
        _trace: Arc<GossipedPayloadTrace>,
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

    async fn update_trusted_builders(
        &self,
        validator_keys: &Vec<BlsPublicKey>,
        _trusted_builders: &Vec<String>,
    ) -> Result<(), DatabaseError> {
        println!("updating trusted builders: {:?}", validator_keys);
        Ok(())
    }

    async fn get_validator_registrations(
        &self,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        Ok(vec![])
    }
}
