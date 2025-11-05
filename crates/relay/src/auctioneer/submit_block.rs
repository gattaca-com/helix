use std::time::Instant;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    self, BuilderInfo, SubmissionTrace, bid_submission::OptimisticVersion,
    metrics::HYDRATION_CACHE_HITS, record_submission_step,
};
use helix_types::{
    MergeableOrdersWithPref, BlockValidationError, SignedBidSubmission, SubmissionVersion,
};
use tokio::sync::oneshot;
use tracing::{error, trace};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        BlockMergeRequest,
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
            Ok((validated, optimistic_version, merging_data)) => {
                let res_tx = if optimistic_version.is_optimistic() {
                    let _ = res_tx.send(Ok(()));
                    None
                } else {
                    Some(res_tx)
                };

                self.simulate_and_store(validated, res_tx, slot_data);

                if self.config.block_merging_config.is_enabled &&
                    let Some(data) = merging_data
                {
                    self.request_merged_block(data);
                }
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
    ) -> Result<(ValidatedData<'a>, OptimisticVersion, Option<MergeData>), BuilderApiError> {
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
                    true,
                );
                (OptimisticVersion::V1, is_top_bid)
            } else {
                (OptimisticVersion::NotOptimistic, false)
            };

        let merging_data = submission_data.merging_data.map(|data| MergeData {
            is_top_bid,
            slot: submission.slot().as_u64(),
            block_hash: *submission.block_hash(),
            block_value: submission.value(),
            proposer_fee_recipient: *submission.proposer_fee_recipient(),
            parent_beacon_block_root: payload_attributes.parent_beacon_block_root,
            execution_payload: submission.execution_payload_ref().clone(),
            merging_data: data,
        });

        let validated = ValidatedData {
            submission,
            tx_root: maybe_tx_root,
            payload_attributes,
            version: submission_data.version,
            is_top_bid,
            trace: submission_data.trace,
        };

        Ok((validated, optimistic_version, merging_data))
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
            trace: validated.trace.clone(),
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

    fn request_merged_block(&mut self, merging_data: MergeData) {
        self.block_merger.update(
            merging_data.is_top_bid,
            &merging_data.block_hash,
            merging_data.block_value,
            merging_data.merging_data,
        );

        if self.block_merger.should_request_merge() {
            let Some((mergeable_orders, _)) = self.block_merger.fetch_best_mergeable_orders()
            else {
                error!("failed to get best mergeable orders, this should never happen!");
                return;
            };

            let merge_request = BlockMergeRequest::new(
                merging_data.slot,
                merging_data.block_value,
                merging_data.proposer_fee_recipient,
                &merging_data.execution_payload,
                merging_data.parent_beacon_block_root,
                mergeable_orders,
            );

            self.sim_manager.handle_merge_request(merge_request);
        }
    }
}

pub struct ValidatedData<'a> {
    pub submission: SignedBidSubmission,
    pub tx_root: Option<B256>,
    pub payload_attributes: &'a PayloadAttributesUpdate,
    pub version: SubmissionVersion,
    pub is_top_bid: bool,
    pub trace: SubmissionTrace,
}

pub struct MergeData {
    pub is_top_bid: bool,
    pub slot: u64,
    pub block_hash: B256,
    pub block_value: U256,
    pub proposer_fee_recipient: Address,
    pub parent_beacon_block_root: Option<B256>,
    pub execution_payload: helix_types::ExecutionPayload,
    pub merging_data: MergeableOrdersWithPref,
}
