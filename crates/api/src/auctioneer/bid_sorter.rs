use std::{
    collections::hash_map::Entry,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256};
use bytes::Bytes;
use helix_common::{
    api::builder_api::TopBidUpdate,
    metrics::{TopBidMetrics, BID_SORTER_PROCESS_LATENCY_US},
    record_submission_step_ns,
    utils::{avg_duration, utcnow_ns},
    SubmissionTrace,
};
use helix_types::{BlockMergingPreferences, BlsPublicKeyBytes, SignedBidSubmission};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::info;

#[derive(Clone, Copy)]
pub struct BidEntry {
    /// Timestamp in ns when the bid was received. Assume this is unique across all bids
    on_receive_ns: u64,
    value: U256,

    block_hash: B256,

    block_number: u64,
    parent_hash: B256,
    fee_recipient: Address,

    merging: BlockMergingPreferences,
}

impl BidEntry {
    fn new(
        on_receive_ns: u64,
        submission: &SignedBidSubmission,
        merging: BlockMergingPreferences,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let payload = submission.execution_payload_ref();

        Self {
            on_receive_ns,
            value: bid_trace.value,
            block_hash: payload.block_hash,
            block_number: payload.block_number,
            parent_hash: payload.parent_hash,
            fee_recipient: payload.fee_recipient,
            merging,
        }
    }
}

#[derive(Default)]
struct BidSorterTelemetry {
    subs: u32,
    top_bids: u32,
    /// Latency to created builder bid struct (tx root)
    subs_create_time: Duration,
    /// Internal bid processing time of the sorter
    subs_process_time: Duration,
}

pub struct BidSorter {
    /// Sender for ws updates, TopBidUpdate SSZ encoded
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    /// Head slot + 1
    curr_bid_slot: u64,
    /// All bid entries for the current slot, used for sorting
    bids: FxHashMap<BlsPublicKeyBytes, BidEntry>,
    /// Demoted builders in this slot for live demotions
    demotions: FxHashSet<BlsPublicKeyBytes>,
    /// Current best bid
    curr_bid: Option<(BlsPublicKeyBytes, BidEntry)>,
    local_telemetry: BidSorterTelemetry,
}

impl BidSorter {
    pub fn new(top_bid_tx: tokio::sync::broadcast::Sender<Bytes>) -> Self {
        Self {
            top_bid_tx,
            curr_bid_slot: 0,
            bids: FxHashMap::with_capacity_and_hasher(250, Default::default()),
            demotions: FxHashSet::with_capacity_and_hasher(50, Default::default()),
            curr_bid: None,
            local_telemetry: BidSorterTelemetry::default(),
        }
    }

    pub fn sort(
        &mut self,
        submission: &SignedBidSubmission,
        trace: &mut SubmissionTrace,
        merging_preferences: BlockMergingPreferences,
        is_optimistic: bool,
    ) {
        let start = Instant::now();
        let bid_trace = submission.bid_trace();
        assert_eq!(bid_trace.slot, self.curr_bid_slot);
        let bid = BidEntry::new(trace.receive, submission, merging_preferences);
        let builder_pubkey = bid_trace.builder_pubkey;
        self.local_telemetry.subs_create_time += start.elapsed();

        let start = Instant::now();
        self.process_header(builder_pubkey, bid, trace, is_optimistic);
        let process_latency = start.elapsed();

        // telemetry
        self.local_telemetry.subs += 1;
        self.local_telemetry.subs_process_time += process_latency;
        BID_SORTER_PROCESS_LATENCY_US.observe(process_latency.as_micros() as f64);
    }

    // TODO: return this from .sort instead
    pub fn is_top_bid(&self, sub: &SignedBidSubmission) -> bool {
        self.curr_bid.as_ref().is_some_and(|c| c.1.block_hash == sub.message().block_hash)
    }

    pub fn best_mergeable(&self) -> Option<B256> {
        let curr = self.curr_bid.as_ref()?;
        curr.1.merging.allow_appending.then(|| curr.1.block_hash)
    }

    pub fn demote(&mut self, demoted: BlsPublicKeyBytes) {
        if !self.demotions.insert(demoted) {
            // already demoted
            return;
        }
        self.process_demotion(demoted);
    }

    pub fn get_header(&self) -> Option<B256> {
        self.curr_bid.as_ref().map(|b| b.1.block_hash)
    }

    fn process_header(
        &mut self,
        new_pubkey: BlsPublicKeyBytes,
        new_bid: BidEntry,
        trace: &mut SubmissionTrace,
        is_optimistic: bool,
    ) {
        match self.bids.entry(new_pubkey) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                if entry.on_receive_ns >= new_bid.on_receive_ns {
                    // stale
                    return;
                } else {
                    *entry = new_bid;
                }
            }

            Entry::Vacant(entry) => {
                entry.insert(new_bid);
            }
        };

        match &self.curr_bid {
            Some((curr_pubkey, curr_bid)) => {
                if new_bid.value > curr_bid.value {
                    self.update_top_bid(new_pubkey, new_bid, Some(trace), is_optimistic);
                } else if new_pubkey == *curr_pubkey {
                    // this was a cancel, need to check all other bids
                    self.traverse_update_top_bid(Some(trace), is_optimistic);
                }
            }

            None => {
                self.update_top_bid(new_pubkey, new_bid, Some(trace), is_optimistic);
            }
        }
    }

    /// This is only for in-slot demotions. For builder that were demoted in a past slot we don't
    /// expect to receive optimistic bids here
    fn process_demotion(&mut self, demoted: BlsPublicKeyBytes) {
        // remove entire entry for this builder
        if self.bids.remove(&demoted).is_none() {
            return;
        };

        if let Some((curr, _)) = &self.curr_bid {
            if *curr == demoted {
                self.traverse_update_top_bid(None, false);
            }
        }
    }

    fn traverse_update_top_bid(
        &mut self,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
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
            self.update_top_bid(best_pk, *best_bid, trace, is_optimistic);
        } else {
            self.curr_bid = None;
        }
    }

    pub(super) fn process_slot(&mut self, bid_slot: u64) {
        if self.curr_bid_slot > 0 {
            self.report();
        }

        self.curr_bid_slot = bid_slot;
        self.bids.clear();
        self.demotions.clear();

        self.curr_bid = None;
    }

    fn update_top_bid(
        &mut self,
        builder_pubkey: BlsPublicKeyBytes,
        bid: BidEntry,
        trace: Option<&mut SubmissionTrace>,
        is_optimistic: bool,
    ) {
        let now_ns = utcnow_ns();

        let top_bid_update = TopBidUpdate {
            timestamp: now_ns / 1_000_000,
            slot: self.curr_bid_slot,
            block_number: bid.block_number,
            block_hash: bid.block_hash,
            parent_hash: bid.parent_hash,
            builder_pubkey,
            fee_recipient: bid.fee_recipient,
            value: bid.value,
        }
        .as_ssz_bytes_fast()
        .into();
        let _ = self.top_bid_tx.send(top_bid_update);

        self.curr_bid = Some((builder_pubkey, bid));

        if let Some(trace) = trace {
            if is_optimistic {
                // this is our "tick to trade" but may be confounded if builder is sending slowly
                record_submission_step_ns("recv_top_bid", trace.receive, now_ns);
                // internal overhead
                record_submission_step_ns("read_body_top_bid", trace.read_body, now_ns);
            }
        }
        self.local_telemetry.top_bids += 1;
        TopBidMetrics::top_bid_update_count();
    }

    pub(super) fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);

        let avg_sub_create = avg_duration(tel.subs_create_time, tel.subs);
        let avg_sub_process = avg_duration(tel.subs_process_time, tel.subs);

        info!(
            slot = self.curr_bid_slot,
            valid_subs = tel.subs,
            ?avg_sub_create,
            ?avg_sub_process,
            top_bids = tel.top_bids,
            "bid sorter telemetry"
        )
    }
}
