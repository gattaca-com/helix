//! Detects when a merge builder has broken an order's atomicity.
//!
//! Not yet wired into [`super::tile`].

use alloy_primitives::B256;
use helix_tcp_types::merging::order::MergeOrderRef;
use rustc_hash::{FxHashMap, FxHashSet};

/// A submitted order's tx hashes in original relative order; `droppable`
/// marks positions that may be entirely absent from the merged block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderTxs {
    hashes: Vec<B256>,
    droppable: FxHashSet<usize>,
}

impl OrderTxs {
    /// Resolves a wire order ref's indices against the submission's own `tx_hashes`.
    pub fn from_ref(order_ref: &MergeOrderRef, tx_hashes: &[B256]) -> Self {
        let resolve = |i: usize| tx_hashes.get(i).copied().unwrap_or_default();
        match order_ref {
            MergeOrderRef::Tx(tx) => OrderTxs {
                hashes: vec![resolve(tx.index as usize)],
                droppable: FxHashSet::default(),
            },
            MergeOrderRef::Bundle(bundle) => OrderTxs {
                hashes: bundle.txs.iter().map(|&i| resolve(i as usize)).collect(),
                droppable: bundle.dropping_txs.iter().map(|&i| i as usize).collect(),
            },
        }
    }

    #[cfg(test)]
    fn new(hashes: Vec<B256>, droppable: impl IntoIterator<Item = usize>) -> Self {
        Self { hashes, droppable: droppable.into_iter().collect() }
    }
}

/// True if every non-droppable hash is present, contiguous and in order.
fn is_satisfied(order: &OrderTxs, positions: &FxHashMap<B256, usize>) -> bool {
    let mut prev: Option<usize> = None;
    for (i, hash) in order.hashes.iter().enumerate() {
        match positions.get(hash) {
            Some(&pos) => {
                if prev.is_some_and(|p| pos != p + 1) {
                    return false;
                }
                prev = Some(pos);
            }
            None if order.droppable.contains(&i) => {}
            None => return false,
        }
    }
    true
}

/// Tx hashes in `final_txs` that belong to some order but aren't explained by
/// any order that was fully honoured. Returned in `final_txs` order.
/// `bundled`/`covered` are caller-owned scratch space, reused across calls.
pub fn find_unbundled_txs(
    final_txs: &[B256],
    orders: &[OrderTxs],
    bundled: &mut Vec<bool>,
    covered: &mut Vec<bool>,
) -> Vec<B256> {
    let positions: FxHashMap<B256, usize> =
        final_txs.iter().enumerate().map(|(i, h)| (*h, i)).collect();

    bundled.clear();
    bundled.resize(final_txs.len(), false);
    covered.clear();
    covered.resize(final_txs.len(), false);
    for order in orders {
        let satisfied = is_satisfied(order, &positions);
        for hash in &order.hashes {
            if let Some(&pos) = positions.get(hash) {
                bundled[pos] = true;
                covered[pos] |= satisfied;
            }
        }
    }

    final_txs
        .iter()
        .enumerate()
        .filter(|&(i, _)| bundled[i] && !covered[i])
        .map(|(_, h)| *h)
        .collect()
}

#[cfg(test)]
mod tests {
    use helix_tcp_types::merging::order::{BundleOrderRef, TxOrderRef};

    use super::*;

    fn hash(b: u8) -> B256 {
        B256::repeat_byte(b)
    }

    fn check(final_txs: &[B256], orders: &[OrderTxs]) -> Vec<B256> {
        find_unbundled_txs(final_txs, orders, &mut Vec::new(), &mut Vec::new())
    }

    #[test]
    fn bundle_fully_present_contiguous_in_order_is_not_unbundled() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], []);
        let final_txs = vec![hash(0), hash(1), hash(2), hash(3), hash(4)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn bundle_missing_required_tx_is_unbundled() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], []);
        let final_txs = vec![hash(1), hash(3)];
        assert_eq!(check(&final_txs, &[order]), vec![hash(1), hash(3)]);
    }

    #[test]
    fn droppable_tx_absent_is_satisfied() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], [1]);
        let final_txs = vec![hash(1), hash(3)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn droppable_tx_present_must_still_be_in_order_and_contiguous() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], [1]);
        let final_txs = vec![hash(1), hash(2), hash(3)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn droppable_tx_present_but_out_of_place_is_unbundled() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], [1]);
        let final_txs = vec![hash(1), hash(99), hash(2), hash(3)];
        assert_eq!(check(&final_txs, &[order]), vec![hash(1), hash(2), hash(3)]);
    }

    #[test]
    fn interleaved_foreign_tx_breaks_contiguity() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], []);
        let final_txs = vec![hash(1), hash(99), hash(2), hash(3)];
        assert_eq!(check(&final_txs, &[order]), vec![hash(1), hash(2), hash(3)]);
    }

    #[test]
    fn reordered_txs_breaks_relative_order() {
        let order = OrderTxs::new(vec![hash(1), hash(2), hash(3)], []);
        let final_txs = vec![hash(2), hash(1), hash(3)];
        assert_eq!(check(&final_txs, &[order]), vec![hash(2), hash(1), hash(3)]);
    }

    #[test]
    fn shared_tx_between_two_bundles_one_satisfied_is_not_flagged() {
        let shared = hash(10);
        let bundle_a = OrderTxs::new(vec![shared, hash(20), hash(30)], []);
        let bundle_b = OrderTxs::new(vec![shared, hash(40)], []);
        let final_txs = vec![shared, hash(40)];
        assert_eq!(check(&final_txs, &[bundle_a, bundle_b]), Vec::<B256>::new());
    }

    #[test]
    fn standalone_tx_order_explains_shared_mev_tx_over_unused_competing_bundles() {
        let mev_tx = hash(10);
        let bundle_b = OrderTxs::new(vec![mev_tx, hash(21)], []);
        let bundle_c = OrderTxs::new(vec![mev_tx, hash(31)], []);
        let standalone_d = OrderTxs::new(vec![mev_tx], []);
        let final_txs = vec![hash(0), mev_tx, hash(99)];
        assert_eq!(check(&final_txs, &[bundle_b, bundle_c, standalone_d]), Vec::<B256>::new());
    }

    #[test]
    fn shared_tx_present_but_neither_bundle_satisfied_is_flagged() {
        let shared = hash(10);
        let bundle_a = OrderTxs::new(vec![shared, hash(20)], []);
        let bundle_b = OrderTxs::new(vec![shared, hash(40)], []);
        let final_txs = vec![shared];
        assert_eq!(check(&final_txs, &[bundle_a, bundle_b]), vec![shared]);
    }

    #[test]
    fn single_tx_order_present_is_never_a_violation() {
        let order = OrderTxs::new(vec![hash(5)], []);
        let final_txs = vec![hash(5), hash(99)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn txs_unrelated_to_any_order_are_ignored() {
        let order = OrderTxs::new(vec![hash(1), hash(2)], []);
        let final_txs = vec![hash(1), hash(2), hash(99), hash(100)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn order_not_used_at_all_is_not_flagged() {
        let order = OrderTxs::new(vec![hash(1), hash(2)], []);
        let final_txs = vec![hash(99)];
        assert_eq!(check(&final_txs, &[order]), Vec::<B256>::new());
    }

    #[test]
    fn from_ref_resolves_tx_order() {
        let tx_hashes = vec![hash(0), hash(1), hash(2)];
        let order_ref = MergeOrderRef::Tx(TxOrderRef { index: 1, can_revert: true });
        let order = OrderTxs::from_ref(&order_ref, &tx_hashes);
        assert_eq!(order, OrderTxs::new(vec![hash(1)], []));
    }

    #[test]
    fn from_ref_resolves_bundle_order_and_dropping_positions() {
        let tx_hashes = vec![hash(0), hash(1), hash(2), hash(3)];
        let order_ref = MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![3, 1, 2],
            reverting_txs: vec![],
            dropping_txs: vec![1],
        });
        let order = OrderTxs::from_ref(&order_ref, &tx_hashes);
        assert_eq!(order, OrderTxs::new(vec![hash(3), hash(1), hash(2)], [1]));
    }

    #[test]
    fn from_ref_out_of_range_index_falls_back_to_default() {
        let tx_hashes = vec![hash(0)];
        let order_ref = MergeOrderRef::Tx(TxOrderRef { index: 99, can_revert: false });
        let order = OrderTxs::from_ref(&order_ref, &tx_hashes);
        assert_eq!(order, OrderTxs::new(vec![B256::default()], []));
    }
}
