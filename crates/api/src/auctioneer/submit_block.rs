use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    self,
    bid_submission::{BidSubmission, OptimisticVersion},
    metrics::HYDRATION_CACHE_HITS,
    utils::utcnow_ns,
    BuilderInfo, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_types::SignedBidSubmission;
use tokio::sync::oneshot;
use tracing::error;

use crate::{
    auctioneer::{
        bid_sorter::BidSorterMessage,
        context::Context,
        simulator::{manager::SimulationResult, BlockSimRequest, SimulatorRequest},
        types::{SlotData, Submission, SubmissionResult},
    },
    builder::error::BuilderApiError,
    Api,
};

impl<A: Api> Context<A> {
    pub(super) fn handle_submission(
        &mut self,
        submission: Submission,
        withdrawals_root: B256,
        sequence: Option<u64>,
        mut trace: SubmissionTrace,
        res_tx: oneshot::Sender<SubmissionResult>,
        slot_data: &SlotData,
    ) {
        match self.validate_and_sort_submission(
            submission,
            withdrawals_root,
            sequence,
            &mut trace,
            slot_data,
        ) {
            Ok((submission, optimistic_version)) => {
                let res_tx = if optimistic_version.is_optimistic() {
                    let _ = res_tx.send(Ok(()));
                    None
                } else {
                    Some(res_tx)
                };
                self.handle_2(submission, trace, optimistic_version, res_tx, slot_data);
            }

            Err(err) => {
                let _ = res_tx.send(Err(err));
            }
        }
    }

    pub(super) fn sort_simulation_result(&mut self, result: &SimulationResult) {
        match &result.result {
            Err(err) if err.is_demotable() => {
                self.bid_sorter.demote(*result.submission.builder_public_key());
            }

            Ok(_) | Err(_) => {
                if !result.was_optimistic_sim() {
                    // TODO: tidy this up, we store the submission not the header
                    let msg = BidSorterMessage::new_from_block_submission(
                        &result.submission,
                        &Default::default(),
                        utcnow_ns(),
                        100,
                        result.submission.withdrawals_root(),
                    );

                    self.bid_sorter.sort(msg);
                }
            }
        }
    }

    fn validate_and_sort_submission(
        &mut self,
        submission: Submission,
        withdrawals_root: B256,
        sequence: Option<u64>,
        trace: &mut SubmissionTrace,
        slot_data: &SlotData,
    ) -> Result<(SignedBidSubmission, OptimisticVersion), BuilderApiError> {
        let submission = match submission {
            Submission::Full(full) => full,
            Submission::Dehydrated(dehydrated) => {
                let (payload, tx_cache_hits, blob_cache_hits) =
                    dehydrated.hydrate(&mut self.hydration_cache)?;

                HYDRATION_CACHE_HITS
                    .with_label_values(&["transaction"])
                    .inc_by(tx_cache_hits as u64);
                HYDRATION_CACHE_HITS.with_label_values(&["blob"]).inc_by(blob_cache_hits as u64);

                payload.validate_payload_ssz_lengths()?;
                payload
            }
        };

        let builder_info = self.builder_info(submission.builder_public_key());

        self.validate_submission(
            &submission,
            &withdrawals_root,
            sequence,
            &builder_info,
            slot_data,
        )?;

        let optimistic_version = if self.can_process_optimistic &&
            self.should_process_optimistically(&submission, &builder_info, slot_data)
        {
            // TODO: tidy this up, we store the submission not the header
            let msg = BidSorterMessage::new_from_block_submission(
                &submission,
                trace,
                utcnow_ns(),
                0,
                withdrawals_root,
            );
            self.bid_sorter.sort(msg);
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };

        Ok((submission, optimistic_version))
    }

    fn handle_2(
        &mut self,
        submission: SignedBidSubmission,
        submission_trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        res_tx: Option<oneshot::Sender<SubmissionResult>>,
        slot_data: &SlotData,
    ) {
        // TODO: pass this from previous step
        let is_top_bid = self.bid_sorter.is_top_bid(&submission);
        let inclusion_list = slot_data.il.clone();

        let request = BlockSimRequest::new(
            slot_data.registration_data.entry.registration.message.gas_limit,
            &submission,
            slot_data.registration_data.entry.preferences.clone(),
            slot_data.payload_attributes.payload_attributes.parent_beacon_block_root,
            inclusion_list,
        );

        // send to sim_manager queue, use another oneshot or simply the sim results?
        let req = SimulatorRequest {
            request,
            on_receive_ns: submission_trace.receive,
            is_top_bid,
            is_optimistic: optimistic_version.is_optimistic(),
            res_tx,
            submission: submission.clone(),
        };
        self.sim_manager.handle_sim_request(req);

        let payload_and_blobs = Arc::new(submission.payload_and_blobs_ref().to_owned());
        self.payloads.insert(*submission.block_hash(), payload_and_blobs);

        let db = self.db.clone();
        tokio::spawn(async move {
            if let Err(err) =
                db.store_block_submission(submission, submission_trace, optimistic_version).await
            {
                error!(%err, "failed to store block submission")
            }
        });
    }

    fn should_process_optimistically(
        &self,
        submission: &SignedBidSubmission,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
    ) -> bool {
        if builder_info.is_optimistic && submission.message().value <= builder_info.collateral {
            if slot_data.registration_data.entry.preferences.filtering.is_regional() &&
                !builder_info.can_process_regional_slot_optimistically()
            {
                return false;
            }

            return true;
        }

        false
    }
}
