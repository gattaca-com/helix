use std::{sync::atomic::Ordering, time::Instant};

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    self, BuilderInfo, SubmissionTrace,
    bid_submission::OptimisticVersion,
    metrics::{BID_ADJUSTMENT_LATENCY, HYDRATION_CACHE_HITS},
    record_submission_step, spawn_tracked,
};
use helix_types::{
    BidAdjustmentData, BlockValidationError, MergeableOrdersWithPref, SeqNum, SignedBidSubmission,
    SubmissionVersion,
};
use tracing::{error, trace};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        BlockSubResultSender,
        bid_adjustor::BidAdjustor,
        context::Context,
        simulator::{BlockSimRequest, SimulatorRequest, manager::SimulationResult},
        types::{PayloadEntry, SlotData, Submission, SubmissionData, SubmissionResult},
    },
    housekeeper::PayloadAttributesUpdate,
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_submission(
        &mut self,
        submission_data: SubmissionData,
        res_tx: BlockSubResultSender<SubmissionResult>,
        slot_data: &SlotData,
    ) {
        let request_id = submission_data.request_id;
        match self.validate_and_sort(submission_data, slot_data) {
            Ok((validated, optimistic_version, merging_data)) => {
                let res_tx = if optimistic_version.is_optimistic() {
                    res_tx.try_send((request_id, Ok(())));
                    None
                } else {
                    Some(res_tx)
                };

                let (req, entry) = self.prep_data_to_store_and_sim(validated, res_tx, slot_data);

                if !self.completed_dry_run &&
                    entry.is_adjustable() &&
                    self.adjustments_enabled.load(Ordering::Relaxed)
                {
                    let start = Instant::now();
                    if let Some((adjusted_block, sim_request, _, strategy)) =
                        self.bid_adjustor.try_apply_adjustments(&entry, slot_data, true)
                    {
                        self.completed_dry_run = true;

                        BID_ADJUSTMENT_LATENCY
                            .with_label_values(&[strategy])
                            .observe(start.elapsed().as_micros() as f64);

                        self.store_data_and_sim(sim_request, adjusted_block, true);
                    }
                }

                self.store_data_and_sim(req, entry, false);

                if self.config.block_merging_config.is_enabled &&
                    let Some(data) = merging_data
                {
                    let base_block = data.block_hash;
                    let is_top_bid = data.is_top_bid;
                    self.block_merger.insert_merge_data(data);
                    if is_top_bid {
                        self.block_merger.update_base_block(base_block);
                    }
                    self.request_merged_block();
                }
            }

            Err(err) => {
                res_tx.try_send((request_id, Err(err)));
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
                    res_tx.try_send((
                        result.request_id,
                        Err(BuilderApiError::BlockSimulation(err.clone())),
                    ));
                };
            }

            Ok(_) | Err(_) => {
                if let Some(res_tx) = res_tx {
                    let is_top_bid = self.bid_sorter.sort(
                        result.version,
                        &result.submission,
                        &mut result.trace,
                        false,
                    );
                    if is_top_bid {
                        self.block_merger.update_base_block(*result.submission.block_hash());
                    }
                    self.request_merged_block();

                    res_tx.try_send((result.request_id, Ok(())));
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
            block_value: *submission.value(),
            proposer_fee_recipient: *submission.proposer_fee_recipient(),
            parent_beacon_block_root: payload_attributes.parent_beacon_block_root,
            execution_payload: submission.execution_payload_ref().clone(),
            merging_data: data,
        });

        let validated = ValidatedData {
            request_id: submission_data.request_id,
            submission,
            tx_root: maybe_tx_root,
            payload_attributes,
            version: submission_data.version,
            is_top_bid,
            trace: submission_data.trace,
            bid_adjustment_data: submission_data.bid_adjustment_data,
        };

        Ok((validated, optimistic_version, merging_data))
    }

    fn prep_data_to_store_and_sim(
        &mut self,
        validated: ValidatedData,
        res_tx: Option<BlockSubResultSender<SubmissionResult>>,
        slot_data: &SlotData,
    ) -> (SimulatorRequest, PayloadEntry) {
        let request = BlockSimRequest::new(
            slot_data.registration_data.entry.registration.message.gas_limit,
            &validated.submission,
            slot_data.registration_data.entry.preferences.clone(),
            validated.payload_attributes.parent_beacon_block_root,
            slot_data.il.clone(),
        );

        let req = SimulatorRequest {
            submission_request_id: validated.request_id,
            request,
            is_top_bid: validated.is_top_bid,
            res_tx,
            submission: validated.submission.clone(),
            trace: validated.trace.clone(),
            tx_root: validated.tx_root,
            version: validated.version,
        };

        let entry = PayloadEntry::new_submission(
            validated.submission,
            validated.payload_attributes.withdrawals_root,
            validated.tx_root,
            validated.bid_adjustment_data,
            validated.version,
            validated.trace,
            validated.payload_attributes.parent_beacon_block_root,
        );

        (req, entry)
    }

    pub fn store_data_and_sim(
        &mut self,
        req: SimulatorRequest,
        entry: PayloadEntry,
        fast_track: bool,
    ) {
        let is_adjusted = entry.is_adjusted();
        let block_hash = *req.submission.block_hash();
        self.payloads.insert(block_hash, entry);

        let sub_clone = req.submission.clone();
        let trace_clone = req.trace.clone();
        let opt_version = req.optimistic_version();

        let db = self.db.clone();
        spawn_tracked!(async move {
            if let Err(err) =
                db.store_block_submission(sub_clone, trace_clone, opt_version, is_adjusted).await
            {
                error!(%err, "failed to store block submission")
            }
        });

        self.sim_manager.handle_sim_request(req, fast_track);
    }

    fn should_process_optimistically(
        &self,
        submission: &SignedBidSubmission,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
    ) -> bool {
        if slot_data.registration_data.entry.preferences.disable_optimistic {
            return false;
        }

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

    fn request_merged_block(&mut self) {
        if let Some(merge_request) = self.block_merger.fetch_merge_request() {
            self.sim_manager.handle_merge_request(merge_request);
        }
    }
}

pub struct ValidatedData<'a> {
    pub request_id: SeqNum,
    pub submission: SignedBidSubmission,
    pub tx_root: Option<B256>,
    pub payload_attributes: &'a PayloadAttributesUpdate,
    pub version: SubmissionVersion,
    pub is_top_bid: bool,
    pub trace: SubmissionTrace,
    pub bid_adjustment_data: Option<BidAdjustmentData>,
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
