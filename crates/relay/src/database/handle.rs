use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::{B256, U256};
use helix_common::{
    DataAdjustmentsEntry, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace, SubmissionTrace,
    ValidatorSummary,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        proposer_api::GetHeaderParams,
    },
    bid_submission::OptimisticVersion,
    utils::alert_discord,
};
use helix_types::{BlsPublicKeyBytes, MergedBlock, SignedBidSubmission};
use tracing::error;

use crate::database::{
    BuilderInfoDocument, SavePayloadParams,
    postgres::postgres_db_service::{DbRequest, PendingBlockSubmissionValue},
};

#[derive(Clone)]
pub struct DbHandle {
    sender: crossbeam_channel::Sender<DbRequest>,
    batch_sender: crossbeam_channel::Sender<PendingBlockSubmissionValue>,
}

impl DbHandle {
    pub fn new(
        sender: crossbeam_channel::Sender<DbRequest>,
        batch_sender: crossbeam_channel::Sender<PendingBlockSubmissionValue>,
    ) -> Self {
        Self { sender, batch_sender }
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

    pub fn store_block_submission(
        &self,
        submission: SignedBidSubmission,
        trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        is_adjusted: bool,
    ) {
        if let Err(err) = self.batch_sender.send(PendingBlockSubmissionValue {
            submission,
            trace,
            optimistic_version,
            is_adjusted,
        }) {
            error!(%err, "failed to store block submission");
        }
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

    pub fn save_block_adjustments_data(&self, entry: DataAdjustmentsEntry) {
        if let Err(err) = self.sender.send(DbRequest::SaveBlockAdjustmentsData { entry }) {
            error!(%err, "failed to send SaveBlockAdjustmentsData request");
        }
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

    pub fn save_merged_blocks(&self, blocks: Vec<MergedBlock>) {
        if let Err(err) = self.sender.try_send(DbRequest::SaveMergedBlocks { blocks }) {
            error!(%err, "failed to send SaveMergedBlocks request");
        }
    }
}
