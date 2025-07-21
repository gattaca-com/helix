use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    map::foldhash::{HashMap, HashSet},
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
    utils::utcnow_ms,
};

type BlsPubkey = [u8; 48];

/// Shared container for get_header response, thread safe
// TODO!!: use arc swap, validate params in load, avoid cloning
#[derive(Clone)]
pub struct BestGetHeader(Arc<RwLock<Option<BuilderBid>>>);

impl BestGetHeader {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    fn store(&self, bid: BuilderBid) {
        *self.0.write() = Some(bid);
    }

    pub fn load(
        &self,
        _slot: u64,
        parent_hash: &B256,
        _validator_pubkey: &BlsPublicKey,
    ) -> Option<BuilderBid> {
        let bid = (*self.0.read()).clone()?;

        if bid.header().parent_hash().0 == *parent_hash {
            return None
        }

        Some(bid)
    }

    fn reset(&self) {
        *self.0.write() = None
    }
}

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
    },
    /// Demotion of a builder pubkey, all its bids are invalidated for this slot
    Demotion(BlsPublicKey),
    /// New slot update
    Slot(u64),
}

impl BidSorterMessage {
    pub fn new_from_block_submission(
        submission: &SignedBidSubmission,
        on_receive_ns: u64,
        is_cancellable: bool,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = Bid { value: bid_trace.value, on_receive_ns };

        let header = bid_submission_to_builder_bid_unsigned(submission);
        Self::Submission {
            bid,
            builder_pubkey: bid_trace.builder_pubkey.serialize(),
            slot: bid_trace.slot,
            header,
            is_cancellable,
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
            if curr_non_cancel.value > bid.value {
                return false;
            }
        }

        if is_cancellable {
            // keep only if it's the latest
            if let Some(curr_cancel) = self.bid_cancel {
                if curr_cancel.on_receive_ns > bid.on_receive_ns {
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

pub struct BidSorter {
    sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
    /// Sender for ws updates, TopBidUpdate SSZ encoded
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    /// Current slot
    curr_slot: u64,
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
    /// TODO: use arc swap
    shared_best_header: BestGetHeader,
    /// TODO: can use atomicu64 with 100 wei precision
    shared_floor: Arc<RwLock<U256>>,
}

impl BidSorter {
    pub fn new(
        sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        shared_best_header: BestGetHeader,
        shared_floor: Arc<RwLock<U256>>,
    ) -> Self {
        Self {
            sorter_rx,
            top_bid_tx,
            curr_slot: 0,
            bids: HashMap::default(),
            headers: HashMap::default(),
            demotions: HashSet::default(),
            curr_bid: None,
            curr_floor: None,
            shared_best_header,
            shared_floor,
        }
    }

    pub fn run(mut self) {
        info!("starting bid sorter");

        loop {
            let Ok(msg) = self.sorter_rx.try_recv() else {
                continue;
            };

            match msg {
                BidSorterMessage::Submission {
                    bid,
                    builder_pubkey,
                    slot,
                    header,
                    is_cancellable,
                } => {
                    if self.curr_slot != slot {
                        return;
                    }

                    if self.demotions.contains(&builder_pubkey) {
                        return;
                    }

                    self.headers.insert(bid.on_receive_ns, header);
                    self.process_header(builder_pubkey, bid, is_cancellable);
                }
                BidSorterMessage::Demotion(demotion) => self.process_demotion(demotion),
                BidSorterMessage::Slot(slot) => self.process_slot(slot),
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
            self.update_floor_bid(new_bid.value);
        }
    }

    /// This is only for in-slot demotions. For builder that were demoted in a past slot we don't
    /// expect to receive optimistic bids here
    fn process_demotion(&mut self, demoted: BlsPublicKey) {
        let pubkey = demoted.serialize();
        if !self.demotions.insert(pubkey) {
            // already demoted
            return;
        }

        // remove entire entry for this builder
        let Some(entry) = self.bids.remove(&pubkey) else {
            return;
        };

        if let Some((curr, _)) = self.curr_bid {
            if curr == pubkey {
                self.traverse_update_top_bid();
            }
        }

        match (entry.bid_non_cancel, self.curr_floor) {
            (Some(bid), Some(floor)) if bid.value == floor => {
                self.traverse_update_floor_bid();
            }

            _ => return,
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

        if let Some(floor) = best {
            self.update_floor_bid(floor);
        }
    }

    fn process_slot(&mut self, slot: u64) {
        // TODO: local telemetry
        self.curr_slot = slot;
        self.bids.clear();
        self.headers.clear();
        self.demotions.clear();

        self.curr_bid = None;
        self.curr_floor = None;

        self.shared_best_header.reset();
        *self.shared_floor.write() = U256::ZERO;
    }

    fn update_top_bid(&mut self, builder_pubkey: BlsPubkey, bid: Bid) {
        let Some(h) = self.headers.get(&bid.on_receive_ns) else {
            // this should never happen
            return;
        };

        let top_bid_update = TopBidUpdate {
            timestamp: utcnow_ms(),
            slot: 0,
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
        self.shared_best_header.store(h.clone());
    }

    fn update_floor_bid(&mut self, floor: U256) {
        self.curr_floor = Some(floor);
        *self.shared_floor.write() = floor;
    }
}

pub fn start_bid_sorter(
    sorter_rx: crossbeam_channel::Receiver<BidSorterMessage>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    shared_best_header: BestGetHeader,
    shared_floor: Arc<RwLock<U256>>,
) {
    let bid_sorter = BidSorter::new(sorter_rx, top_bid_tx, shared_best_header, shared_floor);
    std::thread::spawn(|| bid_sorter.run());
}
