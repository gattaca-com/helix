use std::{
    collections::hash_map::Entry,
    time::{Duration, Instant},
};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt, HashSet, HashSetExt},
    B256, U256,
};
use bytes::Bytes;
use helix_common::{
    api::builder_api::TopBidUpdate,
    bid_submission::BidSubmission,
    bid_submission_to_builder_bid_unsigned,
    metrics::{TopBidMetrics, BID_SORTER_PROCESS_LATENCY_US},
    utils::{avg_duration, utcnow_ns},
    SubmissionTrace,
};
use helix_types::{BlockMergingPreferences, BlsPublicKeyBytes, BuilderBid, SignedBidSubmission};
use tracing::info;

/// Latest cancellable bid
#[derive(Clone, Copy)]
pub struct BidEntry(Bid);

impl BidEntry {
    /// Returns true if the bid triggered an update
    fn maybe_update(&mut self, bid: Bid) -> bool {
        if self.0.on_receive_ns >= bid.on_receive_ns {
            return false;
        }

        self.0 = bid;

        true
    }
}

#[derive(Clone, Copy)]
pub struct Bid {
    value: U256,
    /// Timestamp in ns when the bid was received. Assume this is unique across all bids
    on_receive_ns: u64,
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
    bids: HashMap<BlsPublicKeyBytes, BidEntry>,
    /// All headers received for this slot
    /// on_receive_ns -> (header, merging_preferences)
    headers: HashMap<u64, (BuilderBid, BlockMergingPreferences)>,
    /// Demoted builders in this slot for live demotions
    demotions: HashSet<BlsPublicKeyBytes>,
    /// Current best bid
    curr_bid: Option<(BlsPublicKeyBytes, Bid, BuilderBid)>,
    local_telemetry: BidSorterTelemetry,
}

impl BidSorter {
    pub fn new(top_bid_tx: tokio::sync::broadcast::Sender<Bytes>) -> Self {
        Self {
            top_bid_tx,
            curr_bid_slot: 0,
            bids: HashMap::with_capacity(250),
            headers: HashMap::with_capacity(2500),
            demotions: HashSet::with_capacity(50),
            curr_bid: None,
            local_telemetry: BidSorterTelemetry::default(),
        }
    }

    pub fn sort(
        &mut self,
        submission: &SignedBidSubmission,
        trace: &mut SubmissionTrace,
        withdrawals_root: B256,
        merging_preferences: BlockMergingPreferences,
    ) {
        let start = Instant::now();
        let bid_trace = submission.bid_trace();
        assert_eq!(bid_trace.slot, self.curr_bid_slot);

        let bid = Bid { value: bid_trace.value, on_receive_ns: trace.receive };
        // TODO: this takes 1-5 ms!
        let header = bid_submission_to_builder_bid_unsigned(submission, withdrawals_root);
        let builder_pubkey = bid_trace.builder_pubkey;
        self.local_telemetry.subs_create_time += start.elapsed();

        let start = Instant::now();
        self.headers.insert(bid.on_receive_ns, (header, merging_preferences));
        self.process_header(builder_pubkey, bid, trace);

        let process_latency = start.elapsed();
        trace.sorted = utcnow_ns();

        // telemetry
        self.local_telemetry.subs += 1;
        self.local_telemetry.subs_process_time += process_latency;
        BID_SORTER_PROCESS_LATENCY_US.observe(process_latency.as_micros() as f64);
    }

    // TODO: return this from .sort instead
    pub fn is_top_bid(&self, sub: &SignedBidSubmission) -> bool {
        self.curr_bid.as_ref().is_some_and(|c| c.2.header.block_hash == sub.message().block_hash)
    }

    pub fn best_mergeable(&self) -> Option<BuilderBid> {
        let curr = self.curr_bid.as_ref()?;
        let (_, preferences) = self.headers.get(&curr.1.on_receive_ns)?;

        preferences.allow_appending.then(|| curr.2.clone())
    }

    pub fn demote(&mut self, demoted: BlsPublicKeyBytes) {
        if !self.demotions.insert(demoted) {
            // already demoted
            return;
        }
        self.process_demotion(demoted);
    }

    pub fn get_header(&self) -> Option<BuilderBid> {
        self.curr_bid.as_ref().map(|b| b.2.clone())
    }

    fn process_header(
        &mut self,
        new_pubkey: BlsPublicKeyBytes,
        new_bid: Bid,
        trace: &mut SubmissionTrace,
    ) {
        match self.bids.entry(new_pubkey) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                if !entry.maybe_update(new_bid) {
                    // bid was stale or too low
                    return;
                };
            }

            Entry::Vacant(entry) => {
                entry.insert(BidEntry(new_bid));
            }
        };

        match &self.curr_bid {
            Some((curr_pubkey, curr_bid, _)) => {
                if new_bid.value > curr_bid.value {
                    self.update_top_bid(new_pubkey, new_bid, Some(trace), false);
                } else if new_pubkey == *curr_pubkey {
                    // this was a cancel, need to check all other bids
                    self.traverse_update_top_bid(Some(trace));
                }
            }

            None => {
                self.update_top_bid(new_pubkey, new_bid, Some(trace), false);
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

        if let Some((curr, _, _)) = &self.curr_bid {
            if *curr == demoted {
                self.traverse_update_top_bid(None);
            }
        }
    }

    fn traverse_update_top_bid(&mut self, trace: Option<&mut SubmissionTrace>) {
        let mut best = None;

        for (pk, entry) in self.bids.iter() {
            let bid = entry.0;

            let Some(curr_best) = best else {
                best = Some((*pk, bid));
                continue;
            };

            if bid.value > curr_best.1.value {
                best = Some((*pk, bid))
            }
        }

        if let Some((best_pk, best_bid)) = best {
            self.update_top_bid(best_pk, best_bid, trace, true);
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
        self.headers.clear();
        self.demotions.clear();

        self.curr_bid = None;
    }

    fn update_top_bid(
        &mut self,
        builder_pubkey: BlsPublicKeyBytes,
        bid: Bid,
        trace: Option<&mut SubmissionTrace>,
        is_cancel: bool,
    ) {
        let Some((h, _)) = self.headers.get(&bid.on_receive_ns) else {
            // this should never happen
            return;
        };

        let now_ns = utcnow_ns();

        let top_bid_update = TopBidUpdate {
            timestamp: now_ns / 1_000_000,
            slot: self.curr_bid_slot,
            block_number: h.header.block_number,
            block_hash: h.header.block_hash,
            parent_hash: h.header.parent_hash,
            builder_pubkey,
            fee_recipient: h.header.fee_recipient,
            value: bid.value,
        }
        .as_ssz_bytes_fast()
        .into();
        let _ = self.top_bid_tx.send(top_bid_update);

        self.curr_bid = Some((builder_pubkey, bid, h.clone()));

        if let Some(trace) = trace {
            trace.top_bid_at = Some((now_ns, is_cancel));
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
