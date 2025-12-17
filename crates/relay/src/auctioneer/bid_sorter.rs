use std::{
    collections::hash_map::Entry,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256};
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

#[derive(Clone, Copy)]
pub struct BidEntry {
    version: SubmissionVersion,
    value: U256,

    block_hash: B256,

    block_number: u64,
    parent_hash: B256,
    fee_recipient: Address,
}

impl BidEntry {
    fn new(version: SubmissionVersion, submission: &SignedBidSubmission) -> Self {
        let bid_trace = submission.bid_trace();
        let payload = submission.execution_payload_ref();

        Self {
            version,
            value: bid_trace.value,
            block_hash: payload.block_hash,
            block_number: payload.block_number,
            parent_hash: payload.parent_hash,
            fee_recipient: payload.fee_recipient,
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
    bids: FxHashMap<BlsPublicKeyBytes, BidEntry>,
    /// Current best bid
    curr_bid: Option<(BlsPublicKeyBytes, BidEntry)>,

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
        bid_slot: u64,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
        top_bid_tx: &tokio::sync::broadcast::Sender<TopBidUpdate>,
    ) {
        let mut best = None;

        for (pk, bid) in self.bids.iter() {
            let Some(curr_best) = best else {
                best = Some((*pk, bid));
                continue;
            };

            if bid.value > curr_best.1.value {
                best = Some((*pk, bid))
            }
        }

        if let Some((best_pk, best_bid)) = best {
            self.update_top_bid(bid_slot, best_pk, *best_bid, trace, is_optimistic, top_bid_tx);
        } else {
            self.curr_bid = None;
        }
    }

    fn update_top_bid(
        &mut self,
        bid_slot: u64,
        builder_pubkey: BlsPublicKeyBytes,
        bid: BidEntry,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
        top_bid_tx: &tokio::sync::broadcast::Sender<TopBidUpdate>,
    ) {
        let now_ns = utcnow_ns();

        let top_bid_update = TopBidUpdate {
            timestamp: now_ns / 1_000_000,
            slot: bid_slot,
            block_number: bid.block_number,
            block_hash: bid.block_hash,
            parent_hash: bid.parent_hash,
            builder_pubkey,
            fee_recipient: bid.fee_recipient,
            value: bid.value,
        };

        let _ = top_bid_tx.send(top_bid_update);
        trace!(?builder_pubkey, value =? bid.value, "updating best bid");
        self.curr_bid = Some((builder_pubkey, bid));

        if let Some(trace) = trace {
            if is_optimistic {
                // this is our "tick to trade" but may be confounded if builder is sending slowly
                record_submission_step_ns("recv_top_bid", trace.receive, now_ns);
                record_submission_step_ns("read_body_top_bid", trace.read_body, now_ns);
                record_submission_step_ns("decode_top_bid", trace.decoded, now_ns);
            } else {
                record_submission_step_ns("recv_top_bid_slow", trace.receive, now_ns);
                record_submission_step_ns("read_body_top_bid_slow", trace.read_body, now_ns);
                record_submission_step_ns("decode_top_bid_slow", trace.decoded, now_ns);
            }
        }

        self.top_bids += 1;
        TopBidMetrics::top_bid_update_count();
    }
}

pub struct BidSorter {
    /// Sender for ws updates, TopBidUpdate SSZ encoded
    top_bid_tx: tokio::sync::broadcast::Sender<TopBidUpdate>,
    /// Head slot + 1
    curr_bid_slot: u64,
    /// Parent hash -> fork state
    forks: FxHashMap<B256, ForkState>,
    /// Demoted builders in this slot for live demotions
    demotions: FxHashSet<BlsPublicKeyBytes>,
    local_telemetry: BidSorterTelemetry,
}

impl BidSorter {
    pub fn new(top_bid_tx: tokio::sync::broadcast::Sender<TopBidUpdate>) -> Self {
        Self {
            top_bid_tx,
            curr_bid_slot: 0,
            forks: FxHashMap::default(),
            demotions: FxHashSet::with_capacity_and_hasher(50, Default::default()),
            local_telemetry: BidSorterTelemetry::default(),
        }
    }

    /// Sort the bid and returns whether it became the top bid
    pub fn sort(
        &mut self,
        version: SubmissionVersion,
        submission: &SignedBidSubmission,
        trace: &mut SubmissionTrace,
        is_optimistic: bool,
    ) -> bool {
        trace!(is_optimistic, "sorting submission");

        let bid_trace = submission.bid_trace();
        assert_eq!(bid_trace.slot, self.curr_bid_slot);
        let bid = BidEntry::new(version, submission);
        let builder_pubkey = bid_trace.builder_pubkey;

        let start = Instant::now();
        let is_top_bid = self.process_header(builder_pubkey, bid, trace, is_optimistic);
        let process_latency = start.elapsed();

        // telemetry
        self.local_telemetry.subs += 1;
        self.local_telemetry.subs_process_time += process_latency;
        BID_SORTER_PROCESS_LATENCY_US.observe(process_latency.as_micros() as f64);

        is_top_bid
    }

    pub fn demote(&mut self, demoted: BlsPublicKeyBytes) {
        if !self.demotions.insert(demoted) {
            // already demoted
            return;
        }
        self.process_demotion(demoted);
    }

    pub fn get_header(&self, parent_hash: &B256) -> Option<B256> {
        self.forks.get(parent_hash).and_then(|s| s.curr_bid.as_ref().map(|b| b.1.block_hash))
    }

    fn process_header(
        &mut self,
        new_pubkey: BlsPublicKeyBytes,
        new_bid: BidEntry,
        trace: &mut SubmissionTrace,
        is_optimistic: bool,
    ) -> bool {
        let state = self.forks.entry(new_bid.parent_hash).or_default();
        match state.bids.entry(new_pubkey) {
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
            Some((curr_pubkey, curr_bid)) => {
                if new_bid.value > curr_bid.value {
                    state.update_top_bid(
                        self.curr_bid_slot,
                        new_pubkey,
                        new_bid,
                        Some(trace),
                        is_optimistic,
                        &self.top_bid_tx,
                    );

                    true
                } else if new_pubkey == *curr_pubkey {
                    // this was a cancel, need to check all other bids
                    trace!("cancel submission, traversing");
                    state.traverse_update_top_bid(
                        self.curr_bid_slot,
                        Some(trace),
                        is_optimistic,
                        &self.top_bid_tx,
                    );

                    false
                } else {
                    // new bid lower than best
                    false
                }
            }

            None => {
                state.update_top_bid(
                    self.curr_bid_slot,
                    new_pubkey,
                    new_bid,
                    Some(trace),
                    is_optimistic,
                    &self.top_bid_tx,
                );

                true
            }
        }
    }

    /// This is only for in-slot demotions. For builder that were demoted in a past slot we don't
    /// expect to receive optimistic bids here
    fn process_demotion(&mut self, demoted: BlsPublicKeyBytes) {
        for state in self.forks.values_mut() {
            // remove entire entry for this builder
            if state.bids.remove(&demoted).is_none() {
                continue;
            }

            if let Some((curr, _)) = &state.curr_bid &&
                *curr == demoted
            {
                state.traverse_update_top_bid(self.curr_bid_slot, None, false, &self.top_bid_tx);
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
