use std::sync::Arc;

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, U256,
};
use helix_types::{MergeableOrder, MergeableOrderWithOrigin, MergeableOrders, SignedBidSubmission};
use parking_lot::RwLock;
use tracing::info;

use crate::bid_submission::BidSubmission;

#[derive(Debug, Clone)]
struct BestMergeableOrdersEntry {
    current_slot: u64,
    order_map: HashMap<MergeableOrder, (U256, Address)>,
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

    pub fn load(&self, slot: u64) -> Vec<MergeableOrderWithOrigin> {
        let entry = self.0.read();
        if entry.current_slot != slot {
            return Vec::new();
        }
        // Clone the orders and return them
        entry
            .order_map
            .iter()
            .map(|(order, (_, origin))| MergeableOrderWithOrigin::new(*origin, order.clone()))
            .collect()
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    pub fn insert_orders(&self, slot: u64, bid_value: U256, mergeable_orders: MergeableOrders) {
        let mut entry = self.0.write();
        if entry.current_slot != slot {
            return;
        }
        let origin = mergeable_orders.origin;

        // Insert each order into the order map
        mergeable_orders.orders.into_iter().for_each(|o| {
            entry
                .order_map
                .entry(o)
                // If the order already exists, keep the one with the highest bid
                .and_modify(|e| {
                    if e.0 < bid_value {
                        *e = (bid_value, origin);
                    }
                })
                // Otherwise, insert the new order
                .or_insert((bid_value, origin));
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

        self.shared_best_orders.reset(head_slot);
    }
}

pub fn start_merging_pool(
    pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
    shared_best_orders: BestMergeableOrders,
) {
    let bid_sorter = MergingPool::new(pool_rx, shared_best_orders);
    std::thread::spawn(|| bid_sorter.run());
}
