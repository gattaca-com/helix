use std::sync::atomic::Ordering;

use alloy_primitives::{Address, B256, U256};
use flux::{spine::SpineProducers, timing::Nanos};
use helix_common::{
    self, BuilderInfo,
    bid_submission::OptimisticVersion,
    metrics::{BID_ADJUSTMENT_LATENCY, HYDRATION_CACHE_HITS},
    record_submission_step,
};
use helix_types::{MergeableOrdersWithPref, SignedBidSubmission, Submission};
use tracing::{error, trace};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        bid_adjustor::BidAdjustor,
        bid_sorter::Bid,
        context::{Context, send_submission_result},
        types::{PayloadEntry, SlotData, SubmissionData},
    },
    simulator::{SimRequest, ValidationRequest, tile::ValidationResult},
    spine::{
        HelixSpineProducers,
        messages::{ToSimKind, ToSimMsg},
    },
};

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_submission(
        &mut self,
        submission_data: &SubmissionData,
        decoded_ix: usize,
        slot_data: &SlotData,
        producers: &mut HelixSpineProducers,
    ) {
        let submission_ref = submission_data.submission_ref;

        let builder_info = self.builder_info(submission_data.submission.builder_pubkey());
        tracing::Span::current()
            .record("builder_id", tracing::field::display(builder_info.builder_id()));

        trace!("validating submission");
        let start_val = Nanos::now();
        let payload_attributes = match self.validate_submission(
            &submission_data,
            &builder_info,
            slot_data,
        ) {
            Ok(v) => v,
            Err(e) => {
                send_submission_result(
                    producers,
                    &self.future_results,
                    submission_ref,
                    Err(BuilderApiError::BidValidation(e)),
                );
                return;
            }
        };
        record_submission_step("validated", start_val.elapsed());
        trace!("validated");

        let mut submission_data = submission_data.clone();

        let (optimistic_version, is_top_bid) = if self.accept_optimistic.load(Ordering::Relaxed) &&
            !self.failsafe_triggered.load(Ordering::Relaxed) &&
            self.should_process_optimistically(&submission_data, &builder_info, slot_data)
        {
            let bid = Bid::from_submission_data(&submission_data);
            let is_top_bid = self.bid_sorter.sort(bid, &mut submission_data.trace, true, producers);
            (OptimisticVersion::V1, is_top_bid)
        } else {
            (OptimisticVersion::NotOptimistic, false)
        };

        let is_optimistic = optimistic_version.is_optimistic();
        if is_optimistic {
            send_submission_result(producers, &self.future_results, submission_ref, Ok(()));
        }

        let req = ValidationRequest {
            is_top_bid,
            is_optimistic,
            apply_blacklist: slot_data.registration_data.entry.preferences.filtering.is_regional(),
            registered_gas_limit: slot_data.registration_data.entry.registration.message.gas_limit,
            parent_beacon_block_root: payload_attributes
                .parent_beacon_block_root
                .unwrap_or_default(),
            inclusion_list: slot_data.il.clone().unwrap_or_default(),
            decoded_ix,
            receive_ns: submission_data.trace.receive_ns.0,
        };

        self.send_to_sim(req, false, producers);

        let (submission, maybe_tx_root) = match self.hydrate(submission_data.submission) {
            Ok(v) => v,
            Err(e) => {
                error!(?e, "hydration failed after pre-check passed");
                return;
            }
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

        let entry = PayloadEntry::new_submission(
            submission,
            payload_attributes.withdrawals_root,
            maybe_tx_root,
            submission_data.bid_adjustment_data,
            submission_data.version,
            submission_data.trace,
            payload_attributes.parent_beacon_block_root,
        );

        self.try_adjustments_dry_run(&entry, slot_data, producers);
        self.store_data(entry, is_optimistic);
        self.try_merge_block(merging_data, producers);
    }

    fn try_merge_block(
        &mut self,
        merging_data: Option<MergeData>,
        producers: &mut HelixSpineProducers,
    ) {
        if self.config.block_merging_config.is_enabled &&
            let Some(data) = merging_data
        {
            let base_block = data.block_hash;
            let is_top_bid = data.is_top_bid;
            self.block_merger.insert_merge_data(data);
            if is_top_bid {
                self.block_merger.update_base_block(base_block);
            }
            self.request_merged_block(producers);
        }
    }

    fn try_adjustments_dry_run(
        &mut self,
        entry: &PayloadEntry,
        slot_data: &SlotData,
        producers: &mut HelixSpineProducers,
    ) {
        if !self.completed_dry_run &&
            entry.is_adjustable() &&
            self.cache.adjustments_enabled.load(Ordering::Relaxed)
        {
            let start = Nanos::now();
            if let Some((adjusted_block, sim_request, _, strategy)) =
                self.bid_adjustor.try_apply_adjustments(entry, slot_data, true)
            {
                self.completed_dry_run = true;

                BID_ADJUSTMENT_LATENCY
                    .with_label_values(&[strategy])
                    .observe(start.elapsed().as_micros());

                self.store_data(adjusted_block, sim_request.is_optimistic);
                self.send_to_sim(sim_request, true, producers);
            }
        }
    }

    pub(super) fn sort_simulation_result(
        &mut self,
        result: &mut ValidationResult,
        producers: &mut HelixSpineProducers,
    ) -> bool {
        let Some(result) = &mut result.1 else {
            return false;
        };

        let need_send_result = !result.optimistic_version.is_optimistic();
        match &result.result {
            Err(err) if err.is_demotable() => {
                self.bid_sorter.demote(result.bid.builder_pubkey, producers);
                if need_send_result {
                    send_submission_result(
                        producers,
                        &self.future_results,
                        result.submission_ref,
                        Err(BuilderApiError::BlockSimulation(err.clone())),
                    );
                }
            }

            Ok(_) | Err(_) => {
                let block_hash = result.bid.block_hash;
                let is_top_bid =
                    self.bid_sorter.sort(result.bid, &mut result.trace, false, producers);
                if is_top_bid {
                    self.block_merger.update_base_block(block_hash);
                }
                self.request_merged_block(producers);

                if need_send_result {
                    self.db.update_block_submission_live_ts(block_hash, Nanos::now().0);
                    send_submission_result(
                        producers,
                        &self.future_results,
                        result.submission_ref,
                        Ok(()),
                    );
                }
            }
        }

        need_send_result
    }

    pub fn store_data(&mut self, entry: PayloadEntry, is_optimistic: bool) {
        let block_hash = *entry.block_hash();
        let is_adjusted = entry.is_adjusted();

        if let PayloadEntry::Submission(s) = &entry {
            let opt_version = if is_optimistic {
                OptimisticVersion::V1
            } else {
                OptimisticVersion::NotOptimistic
            };
            // For optimistic submissions the bid is live as soon as it is stored.
            // For non-optimistic, live_ts is updated when the simulation result arrives.
            let live_ts = if is_optimistic { Some(Nanos::now().0) } else { None };
            self.db.store_block_submission(
                s.signed_bid_submission.clone(),
                s.submission_trace,
                opt_version,
                is_adjusted,
                live_ts,
            );
        }

        self.payloads.insert(block_hash, entry);
    }

    pub fn send_to_sim(
        &mut self,
        req: ValidationRequest,
        fast_track: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let ix = self.sim_inbound.push(SimRequest::Validate { req: Box::new(req), fast_track });
        producers.produce(ToSimMsg { kind: ToSimKind::Request, ix, bid_slot: 0 });
    }

    fn should_process_optimistically(
        &self,
        submission: &SubmissionData,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
    ) -> bool {
        !slot_data.registration_data.entry.preferences.disable_optimistic &&
            builder_info.is_optimistic &&
            submission.bid_trace().value <= builder_info.collateral &&
            (!slot_data.registration_data.entry.preferences.filtering.is_regional() ||
                builder_info.can_process_regional_slot_optimistically())
    }

    fn request_merged_block(&mut self, producers: &mut HelixSpineProducers) {
        if let Some(merge_request) = self.block_merger.fetch_merge_request() {
            let ix = self.sim_inbound.push(SimRequest::Merge(merge_request));
            producers.produce(ToSimMsg { kind: ToSimKind::Request, ix, bid_slot: 0 });
        }
    }

    fn hydrate(
        &mut self,
        submission: Submission,
    ) -> Result<(SignedBidSubmission, Option<B256>), BuilderApiError> {
        match submission {
            Submission::Full(full) => Ok((full, None)),
            Submission::Dehydrated(dehydrated) => {
                trace!("hydrating submission");
                let start = Nanos::now();
                let max_blobs_per_block = self.chain_info.max_blobs_per_block();

                let hydrated =
                    self.hydration_cache.hydrate(dehydrated, max_blobs_per_block)?;

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

                Ok((hydrated.submission, hydrated.tx_root))
            }
        }
    }
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
