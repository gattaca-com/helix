use std::sync::atomic::Ordering;

use alloy_primitives::{B256, U256};
use flux::{spine::SpineProducers, timing::Nanos};
use flux_profiler::timed;
use helix_common::{
    self, BuilderInfo,
    bid_submission::OptimisticVersion,
    metrics::{BID_ADJUSTMENT_LATENCY, HYDRATION_CACHE_HITS},
    record_submission_step,
};
use helix_types::{SignedBidSubmission, Submission};
use tracing::{error, trace};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{
        bid_adjustor::BidAdjustor,
        bid_sorter::Bid,
        context::{Context, send_submission_result},
        types::{PayloadEntry, SlotData, SubmissionData},
    },
    simulator::{SimPriority, ValidationRequest, ValidationResult},
    spine::{
        HelixSpineProducers,
        messages::{BidUpdate, SimUpdate},
    },
};

impl<B: BidAdjustor> Context<B> {
    #[timed]
    pub(super) fn handle_submission(
        &mut self,
        submission_data: &SubmissionData,
        slot_data: &SlotData,
        producers: &mut HelixSpineProducers,
    ) {
        let submission_ref = submission_data.submission_ref;
        let submission_id = submission_data.submission_id;

        let builder_info = self.builder_info(submission_data.submission.builder_pubkey());
        tracing::Span::current()
            .record("builder_id", tracing::field::display(builder_info.builder_id()));

        trace!("validating submission");
        let start_val = Nanos::now();
        let payload_attributes =
            match self.validate_submission(submission_data, &builder_info, slot_data) {
                Ok(v) => v,
                Err(e) => {
                    // The auctioneer's hydration cache must still learn this submission's
                    // txs, otherwise subsequent submissions referencing them fail.
                    let _ = self.hydrate(submission_data.submission.clone());
                    send_submission_result(
                        producers,
                        &self.future_results,
                        submission_id,
                        submission_ref,
                        Err(BuilderApiError::BidValidation(e)),
                    );
                    return;
                }
            };
        record_submission_step("validated", start_val.elapsed());
        trace!("validated");

        let version = submission_data.version;
        let is_pessimistic = submission_data.is_pessimistic;
        let bid_adjustment_data = submission_data.bid_adjustment_data.clone();
        let block_access_list = submission_data.block_access_list.clone();
        let mut trace = submission_data.trace;

        let (submission, maybe_tx_root) = match self.hydrate(submission_data.submission.clone()) {
            Ok(v) => v,
            Err(e) => {
                error!(?e, "hydration failed after pre-check passed");
                send_submission_result(
                    producers,
                    &self.future_results,
                    submission_id,
                    submission_ref,
                    Err(BuilderApiError::InternalError),
                );
                return;
            }
        };

        let (optimistic_version, is_top_bid) = if self.sims.accept_optimistic() &&
            !self.failsafe_triggered.load(Ordering::Relaxed) &&
            self.should_process_optimistically(
                is_pessimistic,
                submission.message.value,
                &builder_info,
                slot_data,
            ) {
            let bid = Bid::new(version, &submission);
            let is_top_bid = self.bid_sorter.sort(bid, &mut trace, true, producers);
            (OptimisticVersion::V1, is_top_bid)
        } else {
            let beats_top_bid = self
                .bid_sorter
                .top_bid_value(&submission.message.parent_hash)
                .is_none_or(|top| submission.message.value > top);
            (OptimisticVersion::NotOptimistic, beats_top_bid)
        };

        let is_optimistic = optimistic_version.is_optimistic();
        if is_optimistic {
            send_submission_result(
                producers,
                &self.future_results,
                submission_id,
                submission_ref,
                Ok(()),
            );
        }

        let req = ValidationRequest {
            priority: sim_priority(is_top_bid, is_optimistic),
            submission_id,
            is_top_bid,
            is_optimistic,
            apply_blacklist: slot_data.registration_data.entry.preferences.filtering.is_regional(),
            registered_gas_limit: slot_data.registration_data.entry.registration.message.gas_limit,
            parent_beacon_block_root: payload_attributes
                .parent_beacon_block_root
                .unwrap_or_default(),
            inclusion_list: slot_data.il.clone().unwrap_or_default(),
            submission: submission.clone(),
            block_access_list: block_access_list.clone(),
            tx_root: maybe_tx_root,
            version,
            trace,
            receive_ns: trace.receive_ns.0,
            submission_ref,
        };

        self.send_to_sim(req, false, producers);

        let entry = PayloadEntry::new_submission(
            submission,
            payload_attributes.withdrawals_root,
            maybe_tx_root,
            bid_adjustment_data,
            block_access_list,
            version,
            trace,
            payload_attributes.parent_beacon_block_root,
        );

        self.try_adjustments_dry_run(&entry, slot_data, producers);
        self.store_data(entry, is_optimistic, submission_id, producers);
    }

    #[timed]
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

                let submission_id = sim_request.submission_id;
                self.store_data(
                    adjusted_block,
                    sim_request.is_optimistic,
                    submission_id,
                    producers,
                );
                self.send_to_sim(sim_request, true, producers);
            }
        }
    }

    #[timed]
    pub(super) fn sort_simulation_result(
        &mut self,
        result: &mut ValidationResult,
        producers: &mut HelixSpineProducers,
    ) -> bool {
        let Some(result) = &mut result.1 else {
            return false;
        };

        let need_send_result = !result.optimistic_version.is_optimistic();
        match &mut result.result {
            Err(err) if err.is_demotable() => {
                if let Some(bid) = &result.bid {
                    self.bid_sorter.demote(bid.builder_pubkey, producers);
                }
                if need_send_result {
                    send_submission_result(
                        producers,
                        &self.future_results,
                        result.submission_id,
                        result.submission_ref,
                        Err(BuilderApiError::BlockSimulation(err.clone())),
                    );
                }
            }

            Err(_) => {
                // Non-demotable error — validity unknown, do not sort or update the top bid.
                if need_send_result {
                    send_submission_result(
                        producers,
                        &self.future_results,
                        result.submission_id,
                        result.submission_ref,
                        Err(BuilderApiError::InternalError),
                    );
                }
            }

            Ok(trace) => {
                let bid = result.bid.as_mut().expect("bid always Some on Ok path");
                let block_hash = bid.block_hash;
                self.bid_sorter.sort(*bid, trace, false, producers);

                if need_send_result {
                    producers
                        .produce(BidUpdate { submission_id: result.submission_id, block_hash });
                    self.db.update_block_submission_live_ts(block_hash, Nanos::now().0);
                    send_submission_result(
                        producers,
                        &self.future_results,
                        result.submission_id,
                        result.submission_ref,
                        Ok(()),
                    );
                }
            }
        }

        need_send_result
    }

    pub fn store_data(
        &mut self,
        entry: PayloadEntry,
        is_optimistic: bool,
        submission_id: uuid::Uuid,
        producers: &mut HelixSpineProducers,
    ) {
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
            let live_ts = if is_optimistic {
                producers.produce(BidUpdate { submission_id, block_hash });
                Some(Nanos::now().0)
            } else {
                None
            };
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
        if let Some(started) = self.sims.dispatch(req, fast_track) {
            producers.produce(SimUpdate::Started(started));
        }
    }

    fn should_process_optimistically(
        &self,
        is_pessimistic: bool,
        value: U256,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
    ) -> bool {
        !is_pessimistic &&
            !slot_data.registration_data.entry.preferences.disable_optimistic &&
            builder_info.is_optimistic &&
            value <= builder_info.collateral &&
            (!slot_data.registration_data.entry.preferences.filtering.is_regional() ||
                builder_info.can_process_regional_slot_optimistically())
    }

    #[timed]
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

                let hydrated = self.hydration_cache.hydrate(dehydrated, max_blobs_per_block)?;

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

                Ok((hydrated.submission, hydrated.tx_root))
            }
        }
    }
}

/// A bid that would be served now outranks everything. Below that a live optimistic bid,
/// which can be served without a simulation, outranks a bid that cannot win at all.
fn sim_priority(is_top_bid: bool, is_optimistic: bool) -> SimPriority {
    if is_top_bid {
        SimPriority::Top
    } else if is_optimistic {
        SimPriority::Sample
    } else {
        SimPriority::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bid_that_would_be_served_outranks_every_other_class() {
        assert_eq!(sim_priority(true, true), SimPriority::Top);
        assert_eq!(sim_priority(true, false), SimPriority::Top);
        assert!(SimPriority::Top > SimPriority::Sample);
        assert!(SimPriority::Sample > SimPriority::Low);
    }

    #[test]
    fn a_live_optimistic_bid_outranks_one_that_cannot_win() {
        assert_eq!(sim_priority(false, true), SimPriority::Sample);
        assert_eq!(sim_priority(false, false), SimPriority::Low);
    }
}
