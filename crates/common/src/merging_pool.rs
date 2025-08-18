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
pub struct BestMergeableOrders(Arc<RwLock<HashMap<MergeableOrder, (U256, Address)>>>);

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(HashMap::with_capacity(5000))))
    }

    pub fn load(&self) -> Vec<MergeableOrderWithOrigin> {
        let order_map = self.0.read();
        order_map
            .iter()
            .map(|(order, (_, origin))| MergeableOrderWithOrigin::new(*origin, order.clone()))
            .collect()
    }

    pub fn insert_orders(&self, bid_value: U256, mergeable_orders: MergeableOrders) {
        let mut order_map = self.0.write();
        let origin = mergeable_orders.origin;

        mergeable_orders.orders.into_iter().for_each(|o| {
            order_map
                .entry(o)
                .and_modify(|e| {
                    if e.0 < bid_value {
                        *e = (bid_value, origin);
                    }
                })
                .or_insert((bid_value, origin));
        });
    }

    fn reset(&self) {
        self.0.write().clear();
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
}

impl MergingPool {
    pub fn new(
        pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
        shared_best_orders: BestMergeableOrders,
    ) -> Self {
        Self { pool_rx, shared_best_orders }
    }

    pub fn run(self) {
        info!("starting merging pool");

        loop {
            let Ok(msg) = self.pool_rx.try_recv() else {
                continue;
            };

            match msg {
                MergingPoolMessage::NewOrders { bid_value, orders } => {
                    info!(?bid_value, "received new mergeable orders");
                    self.shared_best_orders.insert_orders(bid_value, orders);
                }
                MergingPoolMessage::Slot(slot) => {
                    info!(?slot, "received slot update");
                    self.shared_best_orders.reset();
                }
            }
        }
    }
}

pub fn start_merging_pool(
    pool_rx: crossbeam_channel::Receiver<MergingPoolMessage>,
    shared_best_orders: BestMergeableOrders,
) {
    let bid_sorter = MergingPool::new(pool_rx, shared_best_orders);
    std::thread::spawn(|| bid_sorter.run());
}
