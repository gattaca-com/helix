use std::{collections::HashSet, sync::Arc};

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
    builder_info::BuilderInfo,
    deneb::SignedValidatorRegistration,
    simulator::BlockSimError,
    versioned_payload::PayloadAndBlobs,
    GetHeaderTrace, GetPayloadTrace, GossipedHeaderTrace, GossipedPayloadTrace,
    HeaderSubmissionTrace, ProposerInfo, SignedValidatorRegistrationEntry, SubmissionTrace,
    ValidatorPreferences, ValidatorSummary,
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
        pool_name: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn save_validator_registrations(
        &self,
        entries: Vec<ValidatorRegistrationInfo>,
        pool_name: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn is_registration_update_required(
        &self,
        registration: &SignedValidatorRegistration,
    ) -> Result<bool, DatabaseError>;

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
        payload: Arc<PayloadAndBlobs>,
        latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError>;

    async fn store_block_submission(
        &self,
        submission: Arc<SignedBidSubmission>,
        trace: Arc<SubmissionTrace>,
        optimistic_version: i16,
    ) -> Result<(), DatabaseError>;

    async fn store_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
        builder_info: &BuilderInfo,
    ) -> Result<(), DatabaseError>;

    async fn store_builders_info(
        &self,
        builders: &[BuilderInfoDocument],
    ) -> Result<(), DatabaseError>;

    async fn db_get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, DatabaseError>;

    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError>;

    async fn check_builder_api_key(&self, api_key: &str) -> Result<bool, DatabaseError>;

    async fn db_demote_builder(
        &self,
        builder_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        reason: String,
    ) -> Result<(), DatabaseError>;

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
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError>;

    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: ByteVector<32>,
        public_key: BlsPublicKey,
        best_block_hash: ByteVector<32>,
        trace: GetHeaderTrace,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn save_failed_get_payload(
        &self,
        slot: u64,
        block_hash: ByteVector<32>,
        error: String,
        trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError>;

    async fn store_header_submission(
        &self,
        submission: Arc<SignedHeaderSubmission>,
        trace: Arc<HeaderSubmissionTrace>,
    ) -> Result<(), DatabaseError>;

    async fn save_gossiped_header_trace(
        &self,
        block_hash: ByteVector<32>,
        trace: Arc<GossipedHeaderTrace>,
    ) -> Result<(), DatabaseError>;

    async fn save_gossiped_payload_trace(
        &self,
        block_hash: ByteVector<32>,
        trace: Arc<GossipedPayloadTrace>,
    ) -> Result<(), DatabaseError>;

    async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError>;

    async fn get_validator_pool_name(&self, api_key: &str)
        -> Result<Option<String>, DatabaseError>;

    async fn update_trusted_builders(
        &self,
        validator_keys: &[BlsPublicKey],
        trusted_builders: &[String],
    ) -> Result<(), DatabaseError>;

    async fn get_validator_registrations(
        &self,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>;
}
