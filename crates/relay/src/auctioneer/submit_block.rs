use std::time::Instant;

use alloy_primitives::B256;
use helix_common::{
    self, BuilderInfo, SubmissionTrace, bid_submission::OptimisticVersion,
    metrics::HYDRATION_CACHE_HITS, record_submission_step,
};
use helix_types::{
    BlockMergingPreferences, BlockValidationError, SignedBidSubmission, SubmissionVersion,
};
use tokio::sync::oneshot;
use tracing::trace;

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        context::Context,
        simulator::{BlockSimRequest, SimulatorRequest, manager::SimulationResult},
        types::{PayloadEntry, SlotData, Submission, SubmissionData, SubmissionResult},
    },
    housekeeper::PayloadAttributesUpdate,
};

impl Context {
    pub(super) fn handle_submission(
        &mut self,
        submission_data: SubmissionData,
        res_tx: oneshot::Sender<SubmissionResult>,
        slot_data: &SlotData,
    ) {
        match self.validate_and_sort(submission_data, slot_data) {
            Ok((validated, optimistic_version)) => {
                let res_tx = if optimistic_version.is_optimistic() {
                    let _ = res_tx.send(Ok(()));
                    None
                } else {
                    Some(res_tx)
                };

                self.simulate_and_store(validated, res_tx, slot_data);
            }

            Err(err) => {
                let _ = res_tx.send(Err(err));
            }
        }
    }

    /// Sort the simulation result if it wasn't optimistic
    pub(super) fn sort_simulation_result(&mut self, result: &mut SimulationResult) {
        let Some(result) = &mut result.1 else {
            return;
        };

        let res_tx = result.res_tx.take();

        match &result.result {
            Err(err) if err.is_demotable() => {
                self.bid_sorter.demote(*result.submission.builder_public_key());
                if let Some(res_tx) = res_tx {
                    let _ = res_tx.send(Err(BuilderApiError::BlockSimulation(err.clone())));
                };
            }

            Ok(_) | Err(_) => {
                if let Some(res_tx) = res_tx {
                    self.bid_sorter.sort(
                        result.version,
                        &result.submission,
                        &mut result.trace,
                        result.merging_preferences,
                        false,
                    );

                    let _ = res_tx.send(Ok(()));
                };
            }
        }
    }

    fn validate_and_sort<'a>(
        &mut self,
        mut submission_data: SubmissionData,
        slot_data: &'a SlotData,
    ) -> Result<(ValidatedData<'a>, OptimisticVersion), BuilderApiError> {
        if submission_data.bid_slot() != self.bid_slot.as_u64() {
            return Err(BuilderApiError::BidValidation(
                BlockValidationError::SubmissionForWrongSlot {
                    expected: self.bid_slot,
                    got: submission_data.bid_slot().into(),
                },
            ));
        }

        let (submission, maybe_tx_root) = match submission_data.submission {
            Submission::Full(full) => (full, None),
            Submission::Dehydrated(dehydrated) => {
                trace!("hydrating submission");
                let start = Instant::now();
                let max_blobs_per_block = self.chain_info.max_blobs_per_block();

                let hydrated =
                    dehydrated.hydrate(&mut self.hydration_cache, max_blobs_per_block)?;

                trace!(
                    tx_cache_hits = hydrated.tx_cache_hits,
                    blob_cache_hits = hydrated.blob_cache_hits,
                    "hydration done"
                );
                record_submission_step("hydration", start.elapsed());

                HYDRATION_CACHE_HITS
                    .with_label_values(&["transaction"])
                    .inc_by(hydrated.tx_cache_hits as u64);
                HYDRATION_CACHE_HITS
                    .with_label_values(&["blob"])
                    .inc_by(hydrated.blob_cache_hits as u64);

                hydrated.submission.validate_payload_ssz_lengths(max_blobs_per_block)?;

                (hydrated.submission, hydrated.tx_root)
            }
        };

        let builder_info = self.builder_info(submission.builder_public_key());
        tracing::Span::current()
            .record("builder_id", tracing::field::display(builder_info.builder_id()));

        trace!("validating submission");
        let start_val = Instant::now();
        let payload_attributes = self.validate_submission(
            &submission,
            submission_data.version,
            &submission_data.withdrawals_root,
            &builder_info,
            slot_data,
        )?;
        record_submission_step("validated", start_val.elapsed());
        trace!("validated");

        let (optimistic_version, is_top_bid) =
            if self.sim_manager.can_process_optimistic_submission() &&
                self.should_process_optimistically(&submission, &builder_info, slot_data)
            {
                let is_top_bid = self.bid_sorter.sort(
                    submission_data.version,
                    &submission,
                    &mut submission_data.trace,
                    submission_data.merging_preferences,
                    true,
                );
                (OptimisticVersion::V1, is_top_bid)
            } else {
                (OptimisticVersion::NotOptimistic, false)
            };

        let validated = ValidatedData {
            submission,
            tx_root: maybe_tx_root,
            payload_attributes,
            merging_preferences: submission_data.merging_preferences,
            version: submission_data.version,
            is_top_bid,
            trace: submission_data.trace,
        };

        Ok((validated, optimistic_version))
    }

    fn simulate_and_store(
        &mut self,
        validated: ValidatedData,
        res_tx: Option<oneshot::Sender<SubmissionResult>>,
        slot_data: &SlotData,
    ) {
        let inclusion_list = slot_data.il.clone();

        let request = BlockSimRequest::new(
            slot_data.registration_data.entry.registration.message.gas_limit,
            &validated.submission,
            slot_data.registration_data.entry.preferences.clone(),
            validated.payload_attributes.parent_beacon_block_root,
            inclusion_list,
        );

        let req = SimulatorRequest {
            request,
            is_top_bid: validated.is_top_bid,
            res_tx,
            submission: validated.submission.clone(),
            merging_preferences: validated.merging_preferences,
            trace: validated.trace,
            tx_root: validated.tx_root,
            version: validated.version,
        };

        self.sim_manager.handle_sim_request(req);

        let block_hash = *validated.submission.block_hash();
        let entry = PayloadEntry::new_submission(
            validated.submission,
            validated.payload_attributes.withdrawals_root,
            validated.tx_root,
        );
        self.payloads.insert(block_hash, entry);
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

struct ValidatedData<'a> {
    submission: SignedBidSubmission,
    tx_root: Option<B256>,
    payload_attributes: &'a PayloadAttributesUpdate,
    merging_preferences: BlockMergingPreferences,
    version: SubmissionVersion,
    is_top_bid: bool,
    trace: SubmissionTrace,
}
