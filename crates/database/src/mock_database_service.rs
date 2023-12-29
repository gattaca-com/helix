use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ethereum_consensus::{
    capella,
    primitives::{BlsPublicKey, Hash32},
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
};
use helix_common::api::proposer_api::ValidatorRegistrationInfo;
use helix_common::{
    api::{builder_api::BuilderGetValidatorsResponseEntry, data_api::BidFilters},
    bid_submission::{BidTrace, SignedBidSubmission},
    simulator::BlockSimError,
    BuilderInfo, GetPayloadTrace, SignedValidatorRegistrationEntry, SubmissionTrace,
    ValidatorSummary,
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
    ) -> Result<(), DatabaseError> {
        Ok(())
    }
    async fn save_validator_registrations(
        &self,
        entries: Vec<ValidatorRegistrationInfo>,
    ) -> Result<(), DatabaseError> {
        Ok(())
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
        _public_keys: Vec<BlsPublicKey>,
    ) -> Result<HashSet<BlsPublicKey>, DatabaseError> {
        Ok(HashSet::new())
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
        _payload: Arc<ExecutionPayload>,
        _latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_block_submission(
        &self,
        _submission: Arc<SignedBidSubmission>,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn save_block_submission_trace(
        &self,
        _block_hash: Hash32,
        _trace: SubmissionTrace,
    ) -> Result<(), DatabaseError> {
        Ok(())
    }

    async fn store_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKey,
        _builder_info: BuilderInfo,
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

    async fn db_demote_builder(
        &self,
        _builder_pub_key: &BlsPublicKey,
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
        Ok(vec![BidSubmissionDocument::default()])
    }

    async fn get_delivered_payloads(
        &self,
        _filters: &BidFilters,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        let doc = DeliveredPayloadDocument {
            bid_trace: Default::default(),
            payload: Arc::new(ExecutionPayload::Capella(capella::ExecutionPayload::default())),
            latency_trace: Default::default(),
        };

        Ok(vec![doc])
    }
}
