use std::sync::{Arc, RwLock};

use alloy_primitives::{
    map::foldhash::{HashMap, HashSet},
    B256, U256,
};
use bytes::Bytes;
use helix_types::{BlsPublicKey, BuilderBid, SignedBidSubmission};
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
#[derive(Clone)]
pub struct BestGetHeader(Arc<RwLock<BuilderBid>>);

impl BestGetHeader {
    pub fn new() -> Self {
        todo!()
    }

    pub fn store(&self, bid: BuilderBid) {
        *self.0.write().unwrap() = bid;
    }

    pub fn load(
        &self,
        _slot: u64,
        _parent_hash: &B256,
        _validator_pubkey: &BlsPublicKey,
    ) -> Option<BuilderBid> {
        None
    }
}

pub enum BidSortMessage {
    /// Pre-validated header submission that is ready to be processed. Headers could come from:
    /// - V1 submissions, either optimistic or non-optimistic after simulation
    /// - V2/V3 submissions
    Submission { bid: BidInfo, header: BuilderBid, is_optimistic: bool, is_cancellable: bool },
    /// Demotion of a builder pubkey, all its bids are invalidated for this slot
    Demotion(BlsPublicKey),
    /// New slot update
    Slot(u64),
}

impl BidSortMessage {
    pub fn new_from_block_submission(
        submission: &SignedBidSubmission,
        on_receive_ns: u64,
        is_optimistic: bool,
        is_cancellable: bool,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = BidInfo {
            builder_pubkey: bid_trace.builder_pubkey.serialize(),
            value: bid_trace.value,
            on_receive_ns,
            slot: bid_trace.slot,
        };

        let header = bid_submission_to_builder_bid_unsigned(submission);
        Self::Submission { bid, header, is_optimistic, is_cancellable }
    }

    pub fn new_from_header_submission(
        submission: &SignedHeaderSubmission,
        on_receive_ns: u64,
        is_optimistic: bool,
        is_cancellable: bool,
    ) -> Self {
        let bid_trace = submission.bid_trace();
        let bid = BidInfo {
            builder_pubkey: bid_trace.builder_pubkey.serialize(),
            value: bid_trace.value,
            on_receive_ns,
            slot: bid_trace.slot,
        };

        let header = header_submission_to_builder_bid_unsigned(submission);
        Self::Submission { bid, header, is_optimistic, is_cancellable }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BidInfo {
    builder_pubkey: BlsPubkey,
    value: U256,
    /// Timestamp when the bid was received
    ///
    /// Assume this is unique per bid
    on_receive_ns: u64,
    slot: u64,
}

impl Default for BidInfo {
    fn default() -> Self {
        Self {
            builder_pubkey: [0; 48],
            value: Default::default(),
            on_receive_ns: Default::default(),
            slot: 0,
        }
    }
}

pub struct BidSorter {
    sorter_rx: crossbeam_channel::Receiver<BidSortMessage>,
    /// Sender for ws updates, TopBidUpdate ssz encoded
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    /// Current slot
    curr_slot: u64,
    /// All bids for the current slot, used for sorting
    bids: Vec<BidInfo>,
    /// all headers for this slot
    /// on_receive_ns -> header
    headers: HashMap<u64, BuilderBid>,
    /// Demoted builders in this slot for live demotions
    demotions: HashSet<BlsPubkey>,
    /// current best bid
    curr_bid: BidInfo,
    /// current response for get_header
    ///
    /// TODO: use arc swap
    curr_header: BestGetHeader,
}

impl BidSorter {
    pub fn new(curr_header: BestGetHeader) -> Self {
        // let bids = Vec::with_capacity(200);
        todo!()
    }

    pub fn run(mut self) {
        info!("starting bid sorter");

        loop {
            let Ok(msg) = self.sorter_rx.try_recv() else {
                continue;
            };

            match msg {
                BidSortMessage::Submission { bid, header, is_optimistic, is_cancellable } => {
                    self.process_header(bid, header, is_optimistic, is_cancellable);
                }
                BidSortMessage::Demotion(demotion) => self.process_demotion(demotion),
                BidSortMessage::Slot(slot) => self.process_slot(slot),
            }
        }
    }

    fn process_header(
        &mut self,
        new_bid: BidInfo,
        header: BuilderBid,
        is_optimistic: bool,
        is_cancellable: bool,
    ) {
        if self.demotions.contains(&new_bid.builder_pubkey) && is_optimistic {
            return;
        }

        if self.curr_slot != new_bid.slot {
            return;
        }

        // TODO: floor bid to some atomic

        if let Some(b) = self.bids.iter_mut().find(|b| b.builder_pubkey == new_bid.builder_pubkey) {
            if new_bid.on_receive_ns <= b.on_receive_ns {
                // stale bid
                return;
            }

            b.value = new_bid.value;
            b.on_receive_ns = new_bid.on_receive_ns;
        } else {
            self.bids.push(new_bid);
        }

        self.headers.insert(new_bid.on_receive_ns, header);

        if new_bid.value > self.curr_bid.value {
            // new highest
            self.update_top_bid(new_bid);
        } else if new_bid.builder_pubkey == self.curr_bid.builder_pubkey {
            // cancel, check for highest
            let top_bid = self.traverse_top_bid();
            self.update_top_bid(top_bid);
        }
    }

    fn process_demotion(&mut self, demoted: BlsPublicKey) {
        let pubkey = demoted.serialize();
        if self.demotions.insert(pubkey) {
            if let Some(e) = self.bids.iter_mut().find(|i| i.builder_pubkey == pubkey) {
                e.value = U256::ZERO;
            }
        }

        // find new max
    }

    fn traverse_top_bid(&self) -> BidInfo {
        todo!()
    }

    fn process_slot(&mut self, slot: u64) {
        // TODO: local telemetry
        self.curr_slot = slot;
        self.bids.clear();
        self.headers.clear();
        self.curr_bid = BidInfo::default();
        self.demotions.clear();
    }

    fn update_top_bid(&mut self, t: BidInfo) {
        let Some(h) = self.headers.get(&t.on_receive_ns) else {
            return;
        };

        let top_bid_update = TopBidUpdate {
            timestamp: utcnow_ms(),
            slot: t.slot,
            block_number: h.header().block_number(),
            block_hash: h.header().block_hash().0,
            parent_hash: h.header().parent_hash().0,
            builder_pubkey: BlsPublicKey::deserialize(&t.builder_pubkey).unwrap(), /* FIXME: use
                                                                                    * bytes */
            fee_recipient: h.header().fee_recipient(),
            value: t.value,
        }
        .as_ssz_bytes()
        .into();
        let _ = self.top_bid_tx.send(top_bid_update);

        self.curr_bid = t;
        self.curr_header.store(h.clone());
    }
}

pub fn start_bid_sorter(
    msg_rx: crossbeam_channel::Receiver<BidSortMessage>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    best_header: BestGetHeader,
) {
    let bid_sorter = BidSorter::new(best_header);
    std::thread::spawn(|| bid_sorter.run());
}
