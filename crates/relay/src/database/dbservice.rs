use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::{B256, U256};
use helix_common::{
    DataAdjustmentsEntry, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace, ProposerInfo,
    SignedValidatorRegistrationEntry, SubmissionTrace, ValidatorPreferences, ValidatorSummary,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        data_api::{
            BidFilters, DataAdjustmentsResponse, ProposerHeaderDeliveredParams,
            ProposerHeaderDeliveredResponse,
        },
        proposer_api::GetHeaderParams,
    },
    bid_submission::OptimisticVersion,
    utils::alert_discord,
};
use helix_types::{BlsPublicKeyBytes, SignedBidSubmission, Slot};
use tokio::sync::oneshot;
use tracing::error;

use crate::database::{
    BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument, SavePayloadParams,
    error::DatabaseError,
    postgres::postgres_db_service::{DbRequest, PendingBlockSubmissionValue},
};

#[derive(Clone)]
pub struct DbService {
    sender: crossbeam_channel::Sender<DbRequest>,
    batch_sender: crossbeam_channel::Sender<PendingBlockSubmissionValue>,
}

impl DbService {
    pub fn new(
        sender: crossbeam_channel::Sender<DbRequest>,
        batch_sender: crossbeam_channel::Sender<PendingBlockSubmissionValue>,
    ) -> Self {
        Self { sender, batch_sender }
    }

    pub fn get_validator_registration(
        &self,
        pub_key: BlsPublicKeyBytes,
    ) -> Result<
        oneshot::Receiver<Result<SignedValidatorRegistrationEntry, DatabaseError>>,
        DatabaseError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetValidatorRegistration { pub_key, response: response_tx })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: Vec<BlsPublicKeyBytes>,
    ) -> Result<
        oneshot::Receiver<Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError>>,
        DatabaseError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetValidatorRegistrationsForPubKeys {
                pub_keys,
                response: response_tx,
            })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: BlsPublicKeyBytes,
        payload_hash: B256,
        message_received: u64,
        payload_fetched: u64,
    ) {
        if let Err(err) = self.sender.try_send(DbRequest::SaveTooLateGetPayload {
            slot,
            proposer_pub_key,
            payload_hash,
            message_received,
            payload_fetched,
        }) {
            error!(%err, "failed to send SaveTooLateGetPayload request");
        }
    }

    pub fn save_delivered_payload(&self, params: SavePayloadParams) {
        if let Err(err) = self.sender.try_send(DbRequest::SaveDeliveredPayload { params }) {
            error!(%err, "failed to send SaveDeliveredPayload request");
        }
    }

    pub fn store_builders_info(&self, builders: Vec<BuilderInfoDocument>) {
        if let Err(err) = self.sender.send(DbRequest::StoreBuildersInfo { builders }) {
            error!(%err, "failed to send StoreBuildersInfo request");
        }
    }

    pub fn db_demote_builder(
        &self,
        slot: u64,
        builder_pub_key: BlsPublicKeyBytes,
        block_hash: B256,
        reason: String,
        failsafe_triggered: Arc<AtomicBool>,
    ) {
        if let Err(err) = self.sender.send(DbRequest::DbDemoteBuilder {
            slot,
            builder_pub_key,
            block_hash,
            reason,
            failsafe_triggered: failsafe_triggered.clone(),
        }) {
            error!(%err, "failed to send DbDemoteBuilder request triggering failsafe: stopping all optimistic submissions");
            failsafe_triggered.store(true, Ordering::Relaxed);
            alert_discord(&format!(
                "{} {} {} failed to demote builder in database! Pausing all optmistic submissions",
                builder_pub_key, err, block_hash
            ));
        }
    }

    pub fn get_bids(
        &self,
        filters: BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<oneshot::Receiver<Result<Vec<BidSubmissionDocument>, DatabaseError>>, DatabaseError>
    {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetBids { filters, validator_preferences, response: response_tx })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn store_block_submission(
        &self,
        submission: SignedBidSubmission,
        trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
    ) {
        if let Err(err) = self.batch_sender.send(PendingBlockSubmissionValue {
            submission,
            trace,
            optimistic_version,
        }) {
            error!(%err, "failed to store block submission");
        }
    }

    pub fn get_delivered_payloads(
        &self,
        filters: BidFilters,
        validator_preferences: Arc<ValidatorPreferences>,
    ) -> Result<
        oneshot::Receiver<Result<Vec<DeliveredPayloadDocument>, DatabaseError>>,
        DatabaseError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetDeliveredPayloads {
                filters,
                validator_preferences,
                response: response_tx,
            })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn save_get_header_call(
        &self,
        params: GetHeaderParams,
        best_block_hash: B256,
        value: U256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    ) {
        if let Err(err) = self.sender.send(DbRequest::SaveGetHeaderCall {
            params,
            best_block_hash,
            value,
            trace,
            mev_boost,
            user_agent,
        }) {
            error!(%err, "failed to send SaveGetHeaderCall request");
        }
    }

    pub fn save_failed_get_payload(
        &self,
        slot: u64,
        block_hash: B256,
        error: String,
        trace: GetPayloadTrace,
    ) {
        if let Err(err) =
            self.sender.send(DbRequest::SaveFailedGetPayload { slot, block_hash, error, trace })
        {
            error!(%err, "failed to send SaveFailedGetPayload request");
        }
    }

    pub fn save_gossiped_payload_trace(&self, block_hash: B256, trace: GossipedPayloadTrace) {
        if let Err(err) =
            self.sender.send(DbRequest::SaveGossipedPayloadTrace { block_hash, trace })
        {
            error!(%err, "failed to send SaveGossipedPayloadTrace request");
        }
    }

    pub fn get_trusted_proposers(
        &self,
    ) -> Result<oneshot::Receiver<Result<Vec<ProposerInfo>, DatabaseError>>, DatabaseError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetTrustedProposers { response: response_tx })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn save_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot: u64,
        block_parent_hash: B256,
        proposer_pubkey: BlsPublicKeyBytes,
    ) {
        if let Err(err) = self.sender.send(DbRequest::SaveInclusionList {
            inclusion_list,
            slot,
            block_parent_hash,
            proposer_pubkey,
        }) {
            error!(%err, "failed to send SaveInclusionList request");
        }
    }

    pub fn get_block_adjustments_for_slot(
        &self,
        slot: Slot,
    ) -> Result<oneshot::Receiver<Result<Vec<DataAdjustmentsResponse>, DatabaseError>>, DatabaseError>
    {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetBlockAdjustmentsForSlot { slot, response: response_tx })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn save_block_adjustments_data(&self, entry: DataAdjustmentsEntry) {
        if let Err(err) = self.sender.send(DbRequest::SaveBlockAdjustmentsData { entry }) {
            error!(%err, "failed to send SaveBlockAdjustmentsData request");
        }
    }

    pub fn get_proposer_header_delivered(
        &self,
        params: ProposerHeaderDeliveredParams,
    ) -> Result<
        oneshot::Receiver<Result<Vec<ProposerHeaderDeliveredResponse>, DatabaseError>>,
        DatabaseError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(DbRequest::GetProposerHeaderDelivered { params, response: response_tx })
            .map_err(|_| DatabaseError::ChannelSendError)?;
        Ok(response_rx)
    }

    pub fn set_known_validators(&self, known_validators: Vec<ValidatorSummary>) {
        if let Err(err) = self.sender.try_send(DbRequest::SetKnownValidators { known_validators }) {
            error!(%err, "failed to send SetKnownValidators request");
        }
    }

    pub fn set_proposer_duties(&self, duties: Vec<BuilderGetValidatorsResponseEntry>) {
        if let Err(err) = self.sender.try_send(DbRequest::SetProposerDuties { duties }) {
            error!(%err, "failed to send SetProposerDuties request");
        }
    }

    pub fn disable_adjustments(
        &self,
        block_hash: B256,
        failsafe_trigger: Arc<AtomicBool>,
        adjustments_enabled: Arc<AtomicBool>,
    ) {
        if let Err(err) = self.sender.try_send(DbRequest::DisableAdjustments {
            block_hash,
            failsafe_trigger: failsafe_trigger.clone(),
            adjustments_enabled: adjustments_enabled.clone(),
        }) {
            error!(%err, "failed to send DisableAdjustments request triggering failsafe: stopping all adjustments");
            failsafe_trigger.store(true, Ordering::Relaxed);
            alert_discord(&format!(
                "{} {} failed to disable adjustments in database! Pausing all adjustments",
                block_hash, err
            ));
        }
    }
}
