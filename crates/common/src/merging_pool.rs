use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, U256,
};
use helix_types::{
    BlobsBundle, MergeableOrder, MergeableOrderWithOrigin, MergeableOrders, SignedBidSubmission,
};
use parking_lot::RwLock;
use tracing::info;

use crate::bid_submission::BidSubmission;

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    origin: Address,
    blobs: Vec<(usize, BlobsBundle)>,
}

#[derive(Debug, Clone)]
struct BestMergeableOrdersEntry {
    current_slot: u64,
    order_map: HashMap<MergeableOrder, OrderMetadata>,
}

#[derive(Debug, Clone)]
pub struct BestMergeableOrders(Arc<RwLock<BestMergeableOrdersEntry>>);

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(BestMergeableOrdersEntry {
            current_slot: 0,
            order_map: HashMap::with_capacity(5000),
        })))
    }

    pub fn load(
        &self,
        slot: u64,
    ) -> (Vec<MergeableOrderWithOrigin>, Vec<(usize, usize, BlobsBundle)>) {
        let entry = self.0.read();
        // If the request is for another slot, return nothing
        if entry.current_slot != slot {
            return (Vec::new(), Vec::new());
        }
        let mut blobs = vec![];
        // Clone the orders and return them, collecting blobs in the process
        let orders = entry
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
    pub fn insert_orders(&self, slot: u64, bid_value: U256, mergeable_orders: MergeableOrders) {
        let mut entry = self.0.write();
        // If the orders are for another slot, discard them
        if entry.current_slot != slot {
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
            match entry.order_map.entry(o) {
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

    fn reset(&self, slot: u64) {
        let mut entry = self.0.write();
        entry.current_slot = slot;
        entry.order_map.clear();
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
    shared_best_orders: BestMergeableOrders,
    curr_bid_slot: u64,
}

impl MergingPool {
    pub fn new(
        pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
        shared_best_orders: BestMergeableOrders,
    ) -> Self {
        Self { pool_rx, shared_best_orders, curr_bid_slot: 0 }
    }

    pub fn run(mut self) {
        info!("starting merging pool");

        loop {
            let Ok(msg) = self.pool_rx.try_recv() else {
                continue;
            };

            match msg {
                MergingPoolMessage::NewOrders { bid_value, orders } => {
                    info!(?bid_value, "received new mergeable orders");
                    self.shared_best_orders.insert_orders(self.curr_bid_slot, bid_value, orders);
                }
                MergingPoolMessage::Slot(head_slot) => self.process_slot(head_slot),
            }
        }
    }

    fn process_slot(&mut self, head_slot: u64) {
        self.curr_bid_slot = head_slot + 1;

        self.shared_best_orders.reset(self.curr_bid_slot);
    }
}

pub fn start_merging_pool(
    pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
    shared_best_orders: BestMergeableOrders,
) {
    let bid_sorter = MergingPool::new(pool_rx, shared_best_orders);
    std::thread::spawn(|| bid_sorter.run());
}
