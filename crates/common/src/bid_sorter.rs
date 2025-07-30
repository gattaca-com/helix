use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt, HashSet, HashSetExt},
    B256, U256,
};
use bytes::Bytes;
use helix_types::{BlsPublicKey, BuilderBid, SignedBidSubmission};
use parking_lot::RwLock;
use ssz::Encode;
use tracing::info;

use crate::{
    api::builder_api::TopBidUpdate,
    bid_submission::{v2::header_submission::SignedHeaderSubmission, BidSubmission},
    bid_submission_to_builder_bid_unsigned, header_submission_to_builder_bid_unsigned,
    metrics::{TopBidMetrics, BID_SORTER_PROCESS_LATENCY_US, BID_SORTER_RECV_LATENCY_US},
    utils::{avg_duration, utcnow_ms, utcnow_ns},
    SubmissionTrace,
};

type BlsPubkey = [u8; 48];

#[derive(Clone)]
struct GetHeaderEntry {
    slot: u64,
    bid: BuilderBid,
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

    fn store(&self, slot: u64, bid: BuilderBid) {
        *self.0.write() = Some(GetHeaderEntry { slot, bid });
    }

    pub fn best_bid(&self, _slot: u64) -> U256 {
        let guard = self.0.read();
        let Some(entry) = guard.as_ref() else {
            return U256::ZERO;
        };

        if entry.slot != _slot {
            return U256::ZERO;
        }

        *entry.bid.value()
    }

    pub fn load(
        &self,
        slot: u64,
        parent_hash: &B256,
        _validator_pubkey: &BlsPublicKey,
    ) -> Option<BuilderBid> {
        let entry = (*self.0.read()).clone()?;

        if entry.slot != slot || entry.bid.header().parent_hash().0 != *parent_hash {
            return None
        }

        Some(entry.bid)
    }

    fn reset(&self) {
        *self.0.write() = None
    }
}

/// Shared container for floor bid, thread safe
#[derive(Clone)]
pub struct FloorBid(Arc<RwLock<(u64, U256)>>);

impl Default for FloorBid {
    fn default() -> Self {
        Self::new()
    }
}

impl FloorBid {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new((0, U256::ZERO))))
    }
    pub fn get(&self, bid_slot: u64) -> U256 {
        let (slot, floor) = *self.0.read();
        if slot == bid_slot {
            floor
        } else {
            U256::ZERO
        }
    }

    fn update(&self, bid_slot: u64, floor: U256) {
        *self.0.write() = (bid_slot, floor);
    }

    fn reset(&self) {
        *self.0.write() = (0, U256::ZERO);
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
        builder_pubkey: BlsPubkey,
        slot: u64,
        header: BuilderBid,
        is_cancellable: bool,
        simulation_time_ns: u64,
    },
    /// Demotion of a builder pubkey, all its bids are invalidated for this slot
    Demotion(BlsPublicKey),
    /// New slot update
    Slot(u64),
}

impl BidSorterMessage {
    pub fn new_from_block_submission(
        submission: &SignedBidSubmission,
        trace: &SubmissionTrace,
        is_cancellable: bool,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = Bid { value: bid_trace.value, on_receive_ns: trace.receive };
        let simulation_time_ns =
            if trace.is_optimistic { 0 } else { trace.simulation.saturating_sub(trace.signature) };

        let header = bid_submission_to_builder_bid_unsigned(submission);
        Self::Submission {
            bid,
            builder_pubkey: bid_trace.builder_pubkey.serialize(),
            slot: bid_trace.slot,
            header,
            is_cancellable,
            simulation_time_ns,
        }
    }

    pub fn new_from_header_submission(
        submission: &SignedHeaderSubmission,
        on_receive_ns: u64,
        is_cancellable: bool,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = Bid { value: bid_trace.value, on_receive_ns };

        let header = header_submission_to_builder_bid_unsigned(submission);
        Self::Submission {
            bid,
            builder_pubkey: bid_trace.builder_pubkey.serialize(),
            slot: bid_trace.slot,
            header,
            is_cancellable,
            simulation_time_ns: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub struct BidEntry {
    /// Latest cancellable bid. Invariant: if `Some` this is always higher than [`bid_non_cancel`]
    bid_cancel: Option<Bid>,
    /// Highest non-cancellable bid
    bid_non_cancel: Option<Bid>,
}

impl BidEntry {
    fn new(bid: Bid, is_cancellable: bool) -> Self {
        if is_cancellable {
            Self { bid_cancel: Some(bid), bid_non_cancel: None }
        } else {
            Self { bid_cancel: None, bid_non_cancel: Some(bid) }
        }
    }

    /// Returns false if the bid didn't trigger an update
    fn maybe_update(&mut self, bid: Bid, is_cancellable: bool) -> bool {
        // check that it's higher than non-cancel bid
        if let Some(curr_non_cancel) = self.bid_non_cancel {
            if curr_non_cancel.value >= bid.value {
                return false;
            }
        }

        if is_cancellable {
            // keep only if it's the latest
            if let Some(curr_cancel) = self.bid_cancel {
                if curr_cancel.on_receive_ns >= bid.on_receive_ns {
                    return false;
                }
            }

            self.bid_cancel = Some(bid);
        } else {
            if let Some(curr_cancel) = self.bid_cancel {
                if curr_cancel.value < bid.value {
                    // invariant: this bid is now too low
                    self.bid_cancel = None;
                }
            }

            self.bid_non_cancel = Some(bid);
        }

        true
    }

    /// Returns the bid entry value, either cancellable or not
    fn bid(&self) -> Option<Bid> {
        // this works because of the invariant above
        self.bid_cancel.or(self.bid_non_cancel)
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
    /// Internal bid processing time of the sorter
    subs_process_time: Duration,
    top_bids: u32,
    non_cancel_bids: u32,
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
    bids: HashMap<BlsPubkey, BidEntry>,
    /// All headers received for this slot
    /// on_receive_ns -> header
    headers: HashMap<u64, BuilderBid>,
    /// Demoted builders in this slot for live demotions
    demotions: HashSet<BlsPubkey>,
    /// Current best bid
    curr_bid: Option<(BlsPubkey, Bid)>,
    /// Current floor bid value
    curr_floor: Option<U256>,
    /// Current response for get_header
    shared_best_header: BestGetHeader,
    /// Current floor bid value
    shared_floor: FloorBid,
    local_telemetry: BidSorterTelemetry,
}

impl BidSorter {
    pub fn new(
        sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        shared_best_header: BestGetHeader,
        shared_floor: FloorBid,
    ) -> Self {
        Self {
            sorter_rx,
            top_bid_tx,
            curr_bid_slot: 0,
            bids: HashMap::with_capacity(250),
            headers: HashMap::with_capacity(2500),
            demotions: HashSet::with_capacity(50),
            curr_bid: None,
            curr_floor: None,
            shared_best_header,
            shared_floor,
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
                    is_cancellable,
                    simulation_time_ns,
                } => {
                    if self.curr_bid_slot != slot {
                        self.local_telemetry.past_subs += 1;
                        continue;
                    }

                    if self.demotions.contains(&builder_pubkey) {
                        self.local_telemetry.demoted_subs += 1;
                        continue;
                    }

                    self.headers.insert(bid.on_receive_ns, header);
                    self.process_header(builder_pubkey, bid, is_cancellable);

                    // telemetry
                    let recv_latency_ns =
                        recv_ns.saturating_sub(bid.on_receive_ns + simulation_time_ns);
                    let process_latency_ns = utcnow_ns().saturating_sub(recv_ns);

                    self.local_telemetry.valid_subs += 1;
                    self.local_telemetry.subs_recv_time += Duration::from_nanos(recv_latency_ns);
                    self.local_telemetry.subs_process_time +=
                        Duration::from_nanos(process_latency_ns);
                    if !is_cancellable {
                        self.local_telemetry.non_cancel_bids += 1;
                    }

                    BID_SORTER_RECV_LATENCY_US.observe(recv_latency_ns as f64 / 1000.);
                    BID_SORTER_PROCESS_LATENCY_US.observe(process_latency_ns as f64 / 1000.);
                }
                BidSorterMessage::Demotion(demoted) => {
                    let demoted = demoted.serialize();
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

    fn process_header(&mut self, new_pubkey: BlsPubkey, new_bid: Bid, is_cancellable: bool) {
        match self.bids.entry(new_pubkey) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                if !entry.maybe_update(new_bid, is_cancellable) {
                    // bid was stale or too low
                    return;
                };
            }

            Entry::Vacant(entry) => {
                entry.insert(BidEntry::new(new_bid, is_cancellable));
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

        if !is_cancellable && new_bid.value > self.curr_floor.unwrap_or_default() {
            self.update_floor_bid(self.curr_bid_slot, new_bid.value);
        }
    }

    /// This is only for in-slot demotions. For builder that were demoted in a past slot we don't
    /// expect to receive optimistic bids here
    fn process_demotion(&mut self, demoted: BlsPubkey) {
        // remove entire entry for this builder
        let Some(entry) = self.bids.remove(&demoted) else {
            return;
        };

        if let Some((curr, _)) = self.curr_bid {
            if curr == demoted {
                self.traverse_update_top_bid();
            }
        }

        match (entry.bid_non_cancel, self.curr_floor) {
            (Some(bid), Some(floor)) if bid.value == floor => {
                self.traverse_update_floor_bid();
            }

            _ => {}
        }
    }

    fn traverse_update_top_bid(&mut self) {
        let mut best = None;

        for (pk, entry) in self.bids.iter() {
            let Some(bid) = entry.bid() else {
                continue;
            };

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

    /// This should be very rare, only if we demote a builder which had the floor bid
    fn traverse_update_floor_bid(&mut self) {
        let mut best = None;

        for (_, entry) in self.bids.iter() {
            let Some(bid_non_cancel) = entry.bid_non_cancel else {
                continue;
            };

            let Some(curr_best) = best else {
                best = Some(bid_non_cancel.value);
                continue;
            };

            if bid_non_cancel.value > curr_best {
                best = Some(bid_non_cancel.value)
            }
        }

        let new_floor = best.unwrap_or(U256::ZERO);
        self.update_floor_bid(self.curr_bid_slot, new_floor);
    }

    fn process_slot(&mut self, head_slot: u64) {
        self.report();

        self.curr_bid_slot = head_slot + 1;
        self.bids.clear();
        self.headers.clear();
        self.demotions.clear();

        self.curr_bid = None;
        self.curr_floor = None;

        self.shared_best_header.reset();
        self.shared_floor.reset();
    }

    fn update_top_bid(&mut self, builder_pubkey: BlsPubkey, bid: Bid) {
        let Some(h) = self.headers.get(&bid.on_receive_ns) else {
            // this should never happen
            return;
        };

        let top_bid_update = TopBidUpdate {
            timestamp: utcnow_ms(),
            slot: self.curr_bid_slot,
            block_number: h.header().block_number(),
            block_hash: h.header().block_hash().0,
            parent_hash: h.header().parent_hash().0,
            builder_pubkey: BlsPublicKey::deserialize(&builder_pubkey).unwrap(), // TODO!!: use PublicKeyBytes
            fee_recipient: h.header().fee_recipient(),
            value: bid.value,
        }
        .as_ssz_bytes()
        .into();
        let _ = self.top_bid_tx.send(top_bid_update);

        self.curr_bid = Some((builder_pubkey, bid));
        self.shared_best_header.store(self.curr_bid_slot, h.clone());

        self.local_telemetry.top_bids += 1;
        TopBidMetrics::top_bid_update_count();
    }

    fn update_floor_bid(&mut self, bid_slot: u64, floor: U256) {
        self.curr_floor = Some(floor);
        self.shared_floor.update(bid_slot, floor);
    }

    fn report(&mut self) {
        let tel = std::mem::take(&mut self.local_telemetry);

        let total_subs = tel.valid_subs + tel.past_subs + tel.demoted_subs;
        let avg_sub_recv = avg_duration(tel.subs_recv_time, total_subs);
        let avg_sub_process = avg_duration(tel.subs_process_time, tel.valid_subs);
        let avg_demotion_process = avg_duration(tel.demotions_process_time, tel.new_demotions);

        info!(
            slot = self.curr_bid_slot,
            valid_subs = tel.valid_subs,
            past_subs = tel.past_subs,
            demoted_subs = tel.demoted_subs,
            ?avg_sub_process,
            ?avg_sub_recv,
            top_bids = tel.top_bids,
            non_cancel_bids = tel.non_cancel_bids,
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
    shared_floor: FloorBid,
) {
    let bid_sorter = BidSorter::new(sorter_rx, top_bid_tx, shared_best_header, shared_floor);
    std::thread::spawn(|| bid_sorter.run());
}
