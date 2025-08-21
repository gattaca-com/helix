use std::collections::hash_map::Entry;

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, U256,
};
use helix_types::{
    BlobsBundle, MergeableOrder, MergeableOrderWithOrigin, MergeableOrders, SignedBidSubmission,
};
use tracing::info;

use crate::bid_submission::BidSubmission;

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    origin: Address,
    blobs: Vec<(usize, BlobsBundle)>,
}

#[derive(Debug, Clone)]
pub struct BestMergeableOrders {
    current_slot: u64,
    // TODO: change structures to avoid copies
    order_map: HashMap<MergeableOrder, OrderMetadata>,
}

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    pub fn new() -> Self {
        Self { current_slot: 0, order_map: HashMap::with_capacity(5000) }
    }

    pub fn load(
        &self,
        slot: u64,
    ) -> (Vec<MergeableOrderWithOrigin>, Vec<(usize, usize, BlobsBundle)>) {
        // If the request is for another slot, return nothing
        if self.current_slot != slot {
            return (Vec::new(), Vec::new());
        }
        let mut blobs = vec![];
        // Clone the orders and return them, collecting blobs in the process
        let orders = self
            .order_map
            .iter()
            .enumerate()
            .map(|(i, (order, metadata))| {
                let order_with_origin =
                    MergeableOrderWithOrigin::new(metadata.origin, order.clone());
                blobs.extend(metadata.blobs.iter().map(|(j, blob)| (i, *j, blob.clone())));
                order_with_origin
            })
            .collect();
        (orders, blobs)
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    pub fn insert_orders(&mut self, slot: u64, bid_value: U256, mergeable_orders: MergeableOrders) {
        // If the orders are for another slot, discard them
        if self.current_slot != slot {
            return;
        }
        let origin = mergeable_orders.origin;

        let mut blob_iter = mergeable_orders.blobs.into_iter().fuse().peekable();
        // Insert each order into the order map
        mergeable_orders.orders.into_iter().enumerate().for_each(|(i, o)| {
            // Collect all blobs for this order
            let mut blobs = vec![];
            while blob_iter.peek().is_some() && blob_iter.peek().unwrap().0 == i {
                let (_i, j, blob) = blob_iter.next().unwrap();
                blobs.push((j, blob));
            }
            match self.order_map.entry(o) {
                // If the order already exists, keep the one with the highest bid
                Entry::Occupied(mut e) if e.get().value < bid_value => {
                    *e.get_mut() = OrderMetadata { value: bid_value, origin, blobs };
                }
                Entry::Occupied(_) => {}
                // Otherwise, insert the new order
                Entry::Vacant(e) => {
                    e.insert(OrderMetadata { value: bid_value, origin, blobs });
                }
            }
        });
    }

    fn reset(&mut self, slot: u64) {
        self.current_slot = slot;
        self.order_map.clear();
    }
}

pub enum MergingPoolMessage {
    /// New mergeable orders received
    NewOrders { bid_value: U256, orders: MergeableOrders },
    /// New slot update
    Slot(u64),
}

impl MergingPoolMessage {
    pub fn new(submission: &SignedBidSubmission, orders: MergeableOrders) -> Self {
        Self::NewOrders { bid_value: submission.value(), orders }
    }
}

pub struct MergingPool {
    pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
    best_orders: BestMergeableOrders,
    curr_bid_slot: u64,
}

impl MergingPool {
    pub fn new(pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>) -> Self {
        let best_orders = BestMergeableOrders::new();
        Self { pool_rx, best_orders, curr_bid_slot: 0 }
    }

    pub fn run(&mut self) {
        info!("starting merging pool");

        loop {
            // Receive all pending messages from channel
            let Ok(msg) = self.pool_rx.try_recv() else {
                return;
            };

            match msg {
                MergingPoolMessage::NewOrders { bid_value, orders } => {
                    info!(?bid_value, "received new mergeable orders");
                    self.best_orders.insert_orders(self.curr_bid_slot, bid_value, orders);
                }
                MergingPoolMessage::Slot(head_slot) => self.process_slot(head_slot),
            }
        }
    }

    fn process_slot(&mut self, head_slot: u64) {
        self.curr_bid_slot = head_slot + 1;

        self.best_orders.reset(self.curr_bid_slot);
    }

    pub fn get_best_orders(
        &self,
        slot: u64,
    ) -> (Vec<MergeableOrderWithOrigin>, Vec<(usize, usize, BlobsBundle)>) {
        self.best_orders.load(slot)
    }
}
