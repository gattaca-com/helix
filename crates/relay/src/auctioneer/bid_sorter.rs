use std::{
    collections::hash_map::Entry, time::{Duration, Instant}
};

use alloy_primitives::{Address, B256, U256};
use flux::spine::SpineProducers;
use helix_common::{
    SubmissionTrace,
    api::builder_api::TopBidUpdate,
    metrics::{BID_SORTER_PROCESS_LATENCY_US, TopBidMetrics},
    record_submission_step_ns,
    utils::{avg_duration, utcnow_ns},
};
use helix_types::{BlsPublicKeyBytes, SignedBidSubmission, SubmissionVersion};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{info, trace};

use crate::{auctioneer::SubmissionData, spine::HelixSpineProducers};

#[derive(Clone, Copy, Debug)]
pub struct Bid {
    pub version: SubmissionVersion,
    pub value: U256,
    pub bid_slot: u64,
    pub block_hash: B256,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub block_number: u64,
    pub parent_hash: B256,
    pub fee_recipient: Address,
}

impl Bid {
    pub fn new(version: SubmissionVersion, submission: &SignedBidSubmission) -> Self {
        let bid_trace = submission.bid_trace();

        Self {
            version,
            value: bid_trace.value,
            bid_slot: bid_trace.slot,
            block_hash: bid_trace.block_hash,
            builder_pubkey: bid_trace.builder_pubkey,
            block_number: submission.block_number(),
            parent_hash: bid_trace.parent_hash,
            fee_recipient: submission.fee_recipient(),
        }
    }

    pub fn from_submission_data(submission: &SubmissionData) -> Self {
        let bid_trace = submission.bid_trace();

        Self {
            version: submission.version,
            value: bid_trace.value,
            bid_slot: bid_trace.slot,
            block_hash: bid_trace.block_hash,
            builder_pubkey: bid_trace.builder_pubkey,
            block_number: submission.block_number(),
            parent_hash: bid_trace.parent_hash,
            fee_recipient: submission.fee_recipient(),
        }
    }
}

#[derive(Default)]
struct BidSorterTelemetry {
    subs: u32,
    /// Internal bid processing time of the sorter
    subs_process_time: Duration,
}

struct ForkState {
    /// All bid entries for the current slot, used for sorting
    bids: FxHashMap<BlsPublicKeyBytes, Bid>,
    /// Current best bid
    curr_bid: Option<Bid>,

    // telemetry
    subs: u32,
    top_bids: u32,
}

impl Default for ForkState {
    fn default() -> Self {
        Self {
            bids: FxHashMap::with_capacity_and_hasher(250, Default::default()),
            curr_bid: None,
            subs: 0,
            top_bids: 0,
        }
    }
}

impl ForkState {
    fn traverse_update_top_bid(
        &mut self,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let mut best = None;

        for bid in self.bids.values() {
            let Some(curr_best) = best else {
                best = Some(bid);
                continue;
            };

            if bid.value > curr_best.value {
                best = Some(bid);
            }
        }

        if let Some(best_bid) = best {
            self.update_top_bid(*best_bid, trace, is_optimistic, producers);
        } else {
            self.curr_bid = None;
        }
    }

    fn update_top_bid(
        &mut self,
        bid: Bid,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
        producers: &mut HelixSpineProducers,
    ) {
        let now_ns = utcnow_ns();

        let top_bid_update = TopBidUpdate {
            timestamp: now_ns / 1_000_000,
            slot: bid.bid_slot,
            block_number: bid.block_number,
            block_hash: bid.block_hash,
            parent_hash: bid.parent_hash,
            builder_pubkey: bid.builder_pubkey,
            fee_recipient: bid.fee_recipient,
            value: bid.value,
        };

        producers.produce(top_bid_update);

        trace!(?bid.builder_pubkey, value =? bid.value, "updating best bid");
        self.curr_bid = Some(bid);

        if let Some(trace) = trace {
            if is_optimistic {
                // this is our "tick to trade" but may be confounded if builder is sending slowly
                record_submission_step_ns("recv_top_bid", trace.receive_ns.0, now_ns);
                record_submission_step_ns("read_body_top_bid", trace.read_body_ns.0, now_ns);
                record_submission_step_ns("decode_top_bid", trace.decoded_ns.0, now_ns);
            } else {
                record_submission_step_ns("recv_top_bid_slow", trace.receive_ns.0, now_ns);
                record_submission_step_ns("read_body_top_bid_slow", trace.read_body_ns.0, now_ns);
                record_submission_step_ns("decode_top_bid_slow", trace.decoded_ns.0, now_ns);
            }
        }

        self.top_bids += 1;
        TopBidMetrics::top_bid_update_count();
    }
}

pub struct BidSorter {
    /// Head slot + 1
    curr_bid_slot: u64,
    /// Parent hash -> fork state
    forks: FxHashMap<B256, ForkState>,
    /// Demoted builders in this slot for live demotions
    demotions: FxHashSet<BlsPublicKeyBytes>,
    local_telemetry: BidSorterTelemetry,
}

impl Default for BidSorter {
    fn default() -> Self {
        Self::new()
    }
}

impl BidSorter {
    pub fn new() -> Self {
        Self {
            curr_bid_slot: 0,
            forks: FxHashMap::default(),
            demotions: FxHashSet::with_capacity_and_hasher(50, Default::default()),
            local_telemetry: BidSorterTelemetry::default(),
        }
    }

    /// Sort the bid and returns whether it became the top bid
    pub fn sort(
        &mut self,
        bid: Bid,
        trace: &mut SubmissionTrace,
        is_optimistic: bool,
        producers: &mut HelixSpineProducers,
    ) -> bool {
        trace!(is_optimistic, "sorting submission");
        assert_eq!(bid.bid_slot, self.curr_bid_slot);

        let start = Instant::now();
        let is_top_bid = self.process_bid(bid, trace, is_optimistic, producers);
        let process_latency = start.elapsed();

        // telemetry
        self.local_telemetry.subs += 1;
        self.local_telemetry.subs_process_time += process_latency;
        BID_SORTER_PROCESS_LATENCY_US.observe(process_latency.as_micros() as f64);

        is_top_bid
    }

    pub fn demote(&mut self, demoted: BlsPublicKeyBytes, producers: &mut HelixSpineProducers) {
        if !self.demotions.insert(demoted) {
            // already demoted
            return;
        }
        self.process_demotion(demoted, producers);
    }

    pub fn get_header(&self, parent_hash: &B256) -> Option<B256> {
        self.forks.get(parent_hash).and_then(|s| s.curr_bid.as_ref().map(|b| b.block_hash))
    }

    fn process_bid(
        &mut self,
        new_bid: Bid,
        trace: &mut SubmissionTrace,
        is_optimistic: bool,
        producers: &mut HelixSpineProducers,
    ) -> bool {
        let state = self.forks.entry(new_bid.parent_hash).or_default();
        match state.bids.entry(new_bid.builder_pubkey) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                if entry.version >= new_bid.version {
                    trace!("bid is stale, ignore");
                    // stale
                    return false;
                } else {
                    *entry = new_bid;
                }
            }

            Entry::Vacant(entry) => {
                entry.insert(new_bid);
            }
        };

        state.subs += 1;
        match &state.curr_bid {
            Some(curr_bid) => {
                if new_bid.value > curr_bid.value {
                    state.update_top_bid(new_bid, Some(trace), is_optimistic, producers);

                    true
                } else if new_bid.builder_pubkey == curr_bid.builder_pubkey {
                    // this was a cancel, need to check all other bids
                    trace!("cancel submission, traversing");
                    state.traverse_update_top_bid(Some(trace), is_optimistic, producers);

                    false
                } else {
                    // new bid lower than best
                    false
                }
            }

            None => {
                state.update_top_bid(new_bid, Some(trace), is_optimistic, producers);

                true
            }
        }
    }

    /// This is only for in-slot demotions. For builder that were demoted in a past slot we don't
    /// expect to receive optimistic bids here
    fn process_demotion(
        &mut self,
        demoted: BlsPublicKeyBytes,
        producers: &mut HelixSpineProducers,
    ) {
        for state in self.forks.values_mut() {
            // remove entire entry for this builder
            if state.bids.remove(&demoted).is_none() {
                continue;
            }

            if let Some(curr_bid) = &state.curr_bid &&
                curr_bid.builder_pubkey == demoted
            {
                state.traverse_update_top_bid(None, false, producers);
            }
        }
    }

    pub(super) fn process_slot(&mut self, bid_slot: u64) {
        if self.curr_bid_slot > 0 {
            self.report();
        }

        self.curr_bid_slot = bid_slot;
        self.demotions.clear();
        self.forks.clear();
    }

    pub(super) fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);

        let avg_sub_process = avg_duration(tel.subs_process_time, tel.subs);
        let fork_report: Vec<_> = self
            .forks
            .iter()
            .map(|(k, s)| format!("parent: {k}, subs: {}, top_bids: {}", s.subs, s.top_bids))
            .collect();

        info!(
            slot = self.curr_bid_slot,
            valid_subs = tel.subs,
            ?avg_sub_process,
            ?fork_report,
            "bid sorter telemetry"
        )
    }
}
