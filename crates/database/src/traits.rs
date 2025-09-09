use std::{collections::HashSet, sync::Arc};

use alloy_primitives::B256;
use async_trait::async_trait;
use helix_common::{
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        data_api::BidFilters,
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{v2::header_submission::SignedHeaderSubmission, OptimisticVersion},
    builder_info::BuilderInfo,
    simulator::BlockSimError,
    GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace, HeaderSubmissionTrace, ProposerInfo,
    SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorPreferences, ValidatorSummary,
};
use helix_types::{
    BlsPublicKeyBytes, PayloadAndBlobs, SignedBidSubmission, SignedValidatorRegistration,
};

use crate::{
    error::DatabaseError,
    types::{BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument},
};

#[async_trait]
pub trait DatabaseService: Send + Sync + Clone {
    async fn save_validator_registrations(
        &self,
        entries: Vec<ValidatorRegistrationInfo>,
        pool_name: Option<String>,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn is_registration_update_required(
        &self,
        registration: &SignedValidatorRegistration,
    ) -> Result<bool, DatabaseError>;

    async fn get_validator_registration(
        &self,
        pub_key: &BlsPublicKeyBytes,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError>;

    async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: &[&BlsPublicKeyBytes],
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>;

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
        public_keys: Vec<BlsPublicKeyBytes>,
    ) -> Result<HashSet<BlsPublicKeyBytes>, DatabaseError>;

    async fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        payload_hash: &B256,
        message_received: u64,
        payload_fetched: u64,
    ) -> Result<(), DatabaseError>;

    async fn save_delivered_payload(
        &self,
        proposer_pub_key: BlsPublicKeyBytes,
        payload: Arc<PayloadAndBlobs>,
        latency_trace: &GetPayloadTrace,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn store_block_submission(
        &self,
        submission: SignedBidSubmission,
        trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
    ) -> Result<(), DatabaseError>;

    async fn store_builder_info(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
        builder_info: &BuilderInfo,
    ) -> Result<(), DatabaseError>;

    async fn store_builders_info(
        &self,
        builders: &[BuilderInfoDocument],
    ) -> Result<(), DatabaseError>;

    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError>;

    async fn check_builder_api_key(&self, api_key: &str) -> Result<bool, DatabaseError>;

    async fn db_demote_builder(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        reason: String,
    ) -> Result<(), DatabaseError>;

    async fn save_simulation_result(
        &self,
        block_hash: B256,
        block_sim_result: Result<(), BlockSimError>,
    ) -> Result<(), DatabaseError>;

    async fn get_bids(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError>;

    async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError>;

    #[allow(clippy::too_many_arguments)]
    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: B256,
        public_key: BlsPublicKeyBytes,
        best_block_hash: B256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    ) -> Result<(), DatabaseError>;

    async fn save_failed_get_payload(
        &self,
        slot: u64,
        block_hash: B256,
        error: String,
        trace: GetPayloadTrace,
    ) -> Result<(), DatabaseError>;

    async fn store_header_submission(
        &self,
        submission: Arc<SignedHeaderSubmission>,
        trace: HeaderSubmissionTrace,
        tx_count: Option<u32>,
    ) -> Result<(), DatabaseError>;

    async fn save_gossiped_payload_trace(
        &self,
        block_hash: B256,
        trace: GossipedPayloadTrace,
    ) -> Result<(), DatabaseError>;

    async fn get_trusted_proposers(&self) -> Result<Vec<ProposerInfo>, DatabaseError>;

    async fn get_validator_pool_name(&self, api_key: &str)
        -> Result<Option<String>, DatabaseError>;

    async fn get_validator_registrations(
        &self,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>;

    async fn save_inclusion_list(
        &self,
        inclusion_list: &InclusionListWithMetadata,
        slot: u64,
        block_parent_hash: &B256,
        proposer_pubkey: &BlsPublicKeyBytes,
    ) -> Result<(), Vec<DatabaseError>>;
}
