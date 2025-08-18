use std::sync::Arc;

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, U256,
};
use helix_types::{MergeableOrder, MergeableOrderWithOrigin, MergeableOrders};
use parking_lot::RwLock;

pub struct MergingPool;

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
