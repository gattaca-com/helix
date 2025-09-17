use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt, HashSet, HashSetExt},
    B256, U256,
};
use bytes::Bytes;
use helix_types::{BlockMergingPreferences, BlsPublicKeyBytes, BuilderBid, SignedBidSubmission};
use parking_lot::RwLock;
use ssz::Encode;
use tracing::info;

use crate::{
    api::builder_api::TopBidUpdate,
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission, BidSubmission, OptimisticVersion,
    },
    bid_submission_to_builder_bid_unsigned, header_submission_to_builder_bid_unsigned,
    metrics::{
        TopBidMetrics, BID_SORTER_PROCESS_LATENCY_US, BID_SORTER_QUEUE_LATENCY_US,
        BID_SORTER_RECV_LATENCY_US,
    },
    utils::{avg_duration, utcnow_ms, utcnow_ns},
    SubmissionTrace,
};

#[derive(Clone)]
struct GetHeaderEntry {
    slot: u64,
    bid: BuilderBid,
    metadata: BlockMergingPreferences,
}

/// Shared container for get_header response, thread safe
#[derive(Clone)]
pub struct BestGetHeader(Arc<RwLock<Option<GetHeaderEntry>>>);

impl Default for BestGetHeader {
    fn default() -> Self {
        Self::new()
    }
}

impl BestGetHeader {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    fn store(&self, slot: u64, bid: BuilderBid, metadata: BlockMergingPreferences) {
        *self.0.write() = Some(GetHeaderEntry { slot, bid, metadata });
    }

    pub fn best_bid(&self, _slot: u64) -> U256 {
        let guard = self.0.read();
        let Some(entry) = guard.as_ref() else {
            return U256::ZERO;
        };

        if entry.slot != _slot {
            return U256::ZERO;
        }

        entry.bid.value
    }

    pub fn load(
        &self,
        slot: u64,
        parent_hash: &B256,
        _validator_pubkey: &BlsPublicKeyBytes,
    ) -> Option<BuilderBid> {
        let entry = (*self.0.read()).clone()?;

        if entry.slot != slot || entry.bid.header.parent_hash != *parent_hash {
            return None;
        }

        Some(entry.bid)
    }

    pub fn load_any(&self, slot: u64) -> Option<(BuilderBid, BlockMergingPreferences)> {
        let entry = (*self.0.read()).clone()?;

        if entry.slot != slot {
            return None;
        }

        Some((entry.bid, entry.metadata))
    }

    fn reset(&self) {
        *self.0.write() = None
    }
}

// ignoring warning because submissions variant will be 99% of messages
#[allow(clippy::large_enum_variant)]
pub enum BidSorterMessage {
    /// Pre-validated submissions ready to be processed. Submissions could come from:
    /// - V1 submissions, either optimistic, or non-optimistic after simulation
    /// - V2/V3 submissions
    Submission {
        bid: Bid,
        builder_pubkey: BlsPublicKeyBytes,
        slot: u64,
        header: BuilderBid,
        /// Preferences related to block merging.
        merging_preferences: BlockMergingPreferences,
        simulation_time_ns: u64,
        before_sorter_ns: u64,
    },
    /// Demotion of a builder pubkey, all its bids are invalidated for this slot
    Demotion(BlsPublicKeyBytes),
    /// New slot update
    Slot(u64),
}

impl BidSorterMessage {
    pub fn new_from_block_submission(
        submission: &SignedBidSubmission,
        trace: &SubmissionTrace,
        optimistic_version: OptimisticVersion,
        merging_preferences: BlockMergingPreferences,
        before_sorter_ns: u64,
        withdrawals_root: B256,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = Bid { value: bid_trace.value, on_receive_ns: trace.receive };
        let simulation_time_ns = if optimistic_version.is_optimistic() {
            0
        } else {
            // this removes also the optimistic check overhead
            trace.simulation.saturating_sub(trace.signature)
        };

        let header = bid_submission_to_builder_bid_unsigned(submission, withdrawals_root);
        Self::Submission {
            bid,
            builder_pubkey: bid_trace.builder_pubkey,
            slot: bid_trace.slot,
            header,
            merging_preferences,
            simulation_time_ns,
            before_sorter_ns,
        }
    }

    pub fn new_from_header_submission(
        submission: &SignedHeaderSubmission,
        on_receive_ns: u64,
        before_sorter_ns: u64,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = Bid { value: bid_trace.value, on_receive_ns };

        let header = header_submission_to_builder_bid_unsigned(submission);
        Self::Submission {
            bid,
            builder_pubkey: bid_trace.builder_pubkey,
            slot: bid_trace.slot,
            header,
            merging_preferences: BlockMergingPreferences::default(),
            simulation_time_ns: 0,
            before_sorter_ns,
        }
    }
}

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
    valid_subs: u32,
    past_subs: u32,
    demoted_subs: u32,
    /// Time when bid was first received to when it arrived in the sorter
    subs_recv_time: Duration,
    /// Time when the bid spent in the queue awaiting processing
    subs_queue_time: Duration,
    /// Internal bid processing time of the sorter
    subs_process_time: Duration,
    top_bids: u32,
    new_demotions: u32,
    duplicate_demotions: u32,
    /// Internal demotion processing time of the sorter
    demotions_process_time: Duration,
}

pub struct BidSorter {
    sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
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
    curr_bid: Option<(BlsPublicKeyBytes, Bid)>,
    /// Current response for get_header
    shared_best_header: BestGetHeader,
    local_telemetry: BidSorterTelemetry,
}

impl BidSorter {
    pub fn new(
        sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        shared_best_header: BestGetHeader,
    ) -> Self {
        Self {
            sorter_rx,
            top_bid_tx,
            curr_bid_slot: 0,
            bids: HashMap::with_capacity(250),
            headers: HashMap::with_capacity(2500),
            demotions: HashSet::with_capacity(50),
            curr_bid: None,
            shared_best_header,
            local_telemetry: BidSorterTelemetry::default(),
        }
    }

    pub fn run(mut self) {
        info!("starting bid sorter");

        loop {
            let Ok(msg) = self.sorter_rx.try_recv() else {
                continue;
            };

            let recv_ns = utcnow_ns();

            match msg {
                BidSorterMessage::Submission {
                    bid,
                    builder_pubkey,
                    slot,
                    header,
                    merging_preferences,
                    simulation_time_ns,
                    before_sorter_ns,
                } => {
                    if self.curr_bid_slot != slot {
                        self.local_telemetry.past_subs += 1;
                        continue;
                    }

                    if self.demotions.contains(&builder_pubkey) {
                        self.local_telemetry.demoted_subs += 1;
                        continue;
                    }

                    self.headers.insert(bid.on_receive_ns, (header, merging_preferences));
                    self.process_header(builder_pubkey, bid);

                    // telemetry
                    let recv_latency_ns =
                        recv_ns.saturating_sub(bid.on_receive_ns + simulation_time_ns);
                    let process_latency_ns = utcnow_ns().saturating_sub(recv_ns);
                    let queue_latency_ns = recv_ns.saturating_sub(before_sorter_ns);

                    self.local_telemetry.valid_subs += 1;
                    self.local_telemetry.subs_recv_time += Duration::from_nanos(recv_latency_ns);
                    self.local_telemetry.subs_process_time +=
                        Duration::from_nanos(process_latency_ns);
                    self.local_telemetry.subs_queue_time += Duration::from_nanos(queue_latency_ns);

                    BID_SORTER_RECV_LATENCY_US.observe(recv_latency_ns as f64 / 1000.);
                    BID_SORTER_PROCESS_LATENCY_US.observe(process_latency_ns as f64 / 1000.);
                    BID_SORTER_QUEUE_LATENCY_US.observe(queue_latency_ns as f64 / 1000.);
                }
                BidSorterMessage::Demotion(demoted) => {
                    if !self.demotions.insert(demoted) {
                        // already demoted
                        self.local_telemetry.duplicate_demotions += 1;
                        continue;
                    }

                    self.process_demotion(demoted);

                    // telemetry
                    self.local_telemetry.new_demotions += 1;
                    self.local_telemetry.demotions_process_time +=
                        Duration::from_nanos(utcnow_ns().saturating_sub(recv_ns));
                }
                BidSorterMessage::Slot(head_slot) => self.process_slot(head_slot),
            }
        }
    }

    fn process_header(&mut self, new_pubkey: BlsPublicKeyBytes, new_bid: Bid) {
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

        match self.curr_bid {
            Some((curr_pubkey, curr_bid)) => {
                if new_bid.value > curr_bid.value {
                    self.update_top_bid(new_pubkey, new_bid);
                } else if new_pubkey == curr_pubkey {
                    // this was a cancel, need to check all other bids
                    self.traverse_update_top_bid();
                }
            }

            None => {
                self.update_top_bid(new_pubkey, new_bid);
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

        if let Some((curr, _)) = self.curr_bid {
            if curr == demoted {
                self.traverse_update_top_bid();
            }
        }
    }

    fn traverse_update_top_bid(&mut self) {
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
            self.update_top_bid(best_pk, best_bid);
        } else {
            self.shared_best_header.reset();
            self.curr_bid = None;
        }
    }

    fn process_slot(&mut self, head_slot: u64) {
        self.report();

        self.curr_bid_slot = head_slot + 1;
        self.bids.clear();
        self.headers.clear();
        self.demotions.clear();

        self.curr_bid = None;

        self.shared_best_header.reset();
    }

    fn update_top_bid(&mut self, builder_pubkey: BlsPublicKeyBytes, bid: Bid) {
        let Some((h, merging_preferences)) = self.headers.get(&bid.on_receive_ns) else {
            // this should never happen
            return;
        };

        let top_bid_update = TopBidUpdate {
            timestamp: utcnow_ms(),
            slot: self.curr_bid_slot,
            block_number: h.header.block_number,
            block_hash: h.header.block_hash,
            parent_hash: h.header.parent_hash,
            builder_pubkey,
            fee_recipient: h.header.fee_recipient,
            value: bid.value,
        }
        .as_ssz_bytes()
        .into();
        let _ = self.top_bid_tx.send(top_bid_update);

        self.curr_bid = Some((builder_pubkey, bid));
        self.shared_best_header.store(self.curr_bid_slot, h.clone(), *merging_preferences);

        self.local_telemetry.top_bids += 1;
        TopBidMetrics::top_bid_update_count();
    }

    fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);

        let total_subs = tel.valid_subs + tel.past_subs + tel.demoted_subs;
        let avg_sub_recv = avg_duration(tel.subs_recv_time, total_subs);
        let avg_sub_process = avg_duration(tel.subs_process_time, tel.valid_subs);
        let avg_sub_queue = avg_duration(tel.subs_queue_time, tel.valid_subs);
        let avg_demotion_process = avg_duration(tel.demotions_process_time, tel.new_demotions);

        info!(
            slot = self.curr_bid_slot,
            valid_subs = tel.valid_subs,
            past_subs = tel.past_subs,
            demoted_subs = tel.demoted_subs,
            ?avg_sub_process,
            ?avg_sub_recv,
            ?avg_sub_queue,
            top_bids = tel.top_bids,
            new_demotions = tel.new_demotions,
            duplicate_demotions = tel.duplicate_demotions,
            ?avg_demotion_process,
            "bid sorter telemetry"
        )
    }
}

pub fn start_bid_sorter(
    sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    shared_best_header: BestGetHeader,
) {
    let bid_sorter = BidSorter::new(sorter_rx, top_bid_tx, shared_best_header);
    std::thread::spawn(|| bid_sorter.run());
}
