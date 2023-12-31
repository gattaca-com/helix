use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use ethereum_consensus::{
    primitives::{BlsPublicKey, Hash32},
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
};

use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, data_api::BidFilters,
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{BidTrace, SignedBidSubmission},
    builder_info::BuilderInfo,
    simulator::BlockSimError,
    GetPayloadTrace, SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorSummary,
};

use crate::{
    error::DatabaseError,
    types::{BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument},
};

#[async_trait]
pub trait DatabaseService: Send + Sync + Clone {
    async fn save_validator_registration(
        &self,
        entry: ValidatorRegistrationInfo,
    ) -> Result<(), DatabaseError>;
    async fn save_validator_registrations(
        &self,
        entries: Vec<ValidatorRegistrationInfo>,
    ) -> Result<(), DatabaseError>;
    async fn get_validator_registration(
        &self,
        pub_key: BlsPublicKey,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError>;
    async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: Vec<BlsPublicKey>,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>;
    async fn get_validator_registration_timestamp(
        &self,
        pub_key: BlsPublicKey,
    ) -> Result<u64, DatabaseError>;

    async fn set_proposer_duties(
        &self,
        proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), DatabaseError>;
    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, DatabaseError>;

    async fn set_known_validators(
        &self,
        known_validators: Vec<ValidatorSummary>,
    ) -> Result<(), DatabaseError>;

    /// Given a list of public keys check if they are known.
    /// Return a list of all pub keys from the list that are known.
    async fn check_known_validators(
        &self,
        public_keys: Vec<BlsPublicKey>,
    ) -> Result<HashSet<BlsPublicKey>, DatabaseError>;

    async fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        payload_hash: &Hash32,
        message_received: u64,
        payload_fetched: u64,
    ) -> Result<(), DatabaseError>;

    async fn save_delivered_payload(
        &self,
        bid_trace: &BidTrace,
        payload: Arc<ExecutionPayload>,
        latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError>;

    async fn store_block_submission(
        &self,
        submission: Arc<SignedBidSubmission>,
    ) -> Result<(), DatabaseError>;

    async fn save_block_submission_trace(
        &self,
        block_hash: Hash32,
        trace: SubmissionTrace,
    ) -> Result<(), DatabaseError>;

    async fn store_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
        builder_info: BuilderInfo,
    ) -> Result<(), DatabaseError>;
    async fn db_get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, DatabaseError>;
    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError>;
    async fn db_demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), DatabaseError>;

    async fn save_simulation_result(
        &self,
        block_hash: ByteVector<32>,
        block_sim_result: Result<(), BlockSimError>,
    ) -> Result<(), DatabaseError>;

    async fn get_bids(
        &self,
        filters: &BidFilters,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError>;
    async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError>;
}
