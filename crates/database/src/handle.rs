use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    DataAdjustmentsEntry, GetHeaderTrace, GetPayloadTrace, GossipedPayloadTrace, SubmissionTrace,
    ValidatorSummary,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        proposer_api::GetHeaderParams,
    },
    bid_submission::OptimisticVersion,
    is_local_dev,
    utils::{alert_discord, utcnow_sec},
};
use helix_types::{BlsPublicKeyBytes, MergedBlock, SignedBidSubmission};
use tracing::{error, warn};

use crate::{
    postgres::postgres_db_service::{DbRequest, PendingBlockSubmissionValue},
    types::{BuilderInfoDocument, SavePayloadParams},
};

static DROPPED_SUBMISSIONS: AtomicU64 = AtomicU64::new(0);
static LAST_DROP_LOG_SEC: AtomicU64 = AtomicU64::new(0);

/// Counts one dropped submission. Returns the number to report when this caller wins the
/// once-a-second log, so a full channel costs one line instead of one line per submission.
fn count_dropped_submission(
    now_sec: u64,
    dropped: &AtomicU64,
    last_log_sec: &AtomicU64,
) -> Option<u64> {
    dropped.fetch_add(1, Ordering::Relaxed);
    let last = last_log_sec.load(Ordering::Relaxed);
    if now_sec <= last {
        return None;
    }
    if last_log_sec.compare_exchange(last, now_sec, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return None;
    }
    Some(dropped.swap(0, Ordering::Relaxed))
}

#[derive(Clone)]
struct DbSender<T> {
    sender: crossbeam_channel::Sender<T>,
}

impl<T> DbSender<T> {
    fn try_send(&self, request: T) -> Result<(), crossbeam_channel::TrySendError<T>> {
        if is_local_dev() {
            warn!("local dev, skipping write to db");
            return Ok(());
        }
        self.sender.try_send(request)
    }
}

// This is temporary until we have the spine connected up
#[derive(Clone)]
pub struct DbHandle {
    sender: DbSender<DbRequest>,
    batch_sender: DbSender<PendingBlockSubmissionValue>,
}

impl DbHandle {
    pub fn new(
        sender: crossbeam_channel::Sender<DbRequest>,
        batch_sender: crossbeam_channel::Sender<PendingBlockSubmissionValue>,
    ) -> Self {
        Self { sender: DbSender { sender }, batch_sender: DbSender { sender: batch_sender } }
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
        if let Err(err) = self.sender.try_send(DbRequest::StoreBuildersInfo { builders }) {
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
        if let Err(err) = self.sender.try_send(DbRequest::DbDemoteBuilder {
            slot,
            builder_pub_key,
            block_hash,
            reason: reason.clone(),
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

    pub fn db_promote_builder(&self, builder_pub_key: BlsPublicKeyBytes) {
        if let Err(err) = self.sender.try_send(DbRequest::DbPromoteBuilder { builder_pub_key }) {
            error!(%err, "failed to send DbPromoteBuilder request");
        }
    }

    pub fn store_block_submission(
        &self,
        submission: SignedBidSubmission,
        trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        is_adjusted: bool,
        live_ts: Option<u64>,
    ) {
        if let Err(err) = self.batch_sender.try_send(PendingBlockSubmissionValue {
            submission,
            trace,
            optimistic_version,
            is_adjusted,
            live_ts,
        }) {
            if let Some(dropped) =
                count_dropped_submission(utcnow_sec(), &DROPPED_SUBMISSIONS, &LAST_DROP_LOG_SEC)
            {
                error!(%err, dropped, "failed to store block submissions");
            }
        }
    }

    pub fn update_block_submission_live_ts(&self, block_hash: B256, live_ts: u64) {
        if let Err(err) =
            self.sender.try_send(DbRequest::UpdateBlockSubmissionLiveTs { block_hash, live_ts })
        {
            error!(%err, %block_hash, "failed to send UpdateBlockSubmissionLiveTs request");
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
        builder_pubkey: BlsPublicKeyBytes,
        proposer_fee_recipient: Address,
        block_number: u64,
        extra_data: Vec<u8>,
    ) {
        if let Err(err) = self.sender.try_send(DbRequest::SaveGetHeaderCall {
            params,
            best_block_hash,
            value,
            trace,
            mev_boost,
            user_agent,
            builder_pubkey,
            proposer_fee_recipient,
            block_number,
            extra_data,
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
            self.sender.try_send(DbRequest::SaveFailedGetPayload { slot, block_hash, error, trace })
        {
            error!(%err, "failed to send SaveFailedGetPayload request");
        }
    }

    pub fn save_gossiped_payload_trace(&self, block_hash: B256, trace: GossipedPayloadTrace) {
        if let Err(err) =
            self.sender.try_send(DbRequest::SaveGossipedPayloadTrace { block_hash, trace })
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
        if let Err(err) = self.sender.try_send(DbRequest::SaveInclusionList {
            inclusion_list,
            slot,
            block_parent_hash,
            proposer_pubkey,
        }) {
            error!(%err, "failed to send SaveInclusionList request");
        }
    }

    pub fn save_block_adjustments_data(&self, entry: DataAdjustmentsEntry) {
        if let Err(err) = self.sender.try_send(DbRequest::SaveBlockAdjustmentsData { entry }) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A full channel must cost one log a second, and no dropped submission may go uncounted.
    #[test]
    fn dropped_submissions_report_once_a_second_and_lose_nothing() {
        let dropped = AtomicU64::new(0);
        let last_log_sec = AtomicU64::new(0);

        assert_eq!(count_dropped_submission(10, &dropped, &last_log_sec), Some(1));
        assert_eq!(count_dropped_submission(10, &dropped, &last_log_sec), None);
        assert_eq!(count_dropped_submission(10, &dropped, &last_log_sec), None);
        assert_eq!(count_dropped_submission(11, &dropped, &last_log_sec), Some(3));
    }
}
