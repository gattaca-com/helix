use helix_types::BidAdjustmentData;

use crate::auctioneer::PayloadEntry;

pub trait BidAdjustor: Send + Sync + Clone + 'static {
    fn try_apply_adjustments(
        &self,
        bid: &PayloadEntry,
        bid_adjustments_data: &Option<BidAdjustmentData>,
    ) -> Option<PayloadEntry>;

    fn on_new_slot(&mut self, bid_slot: u64);
}

#[derive(Clone)]
pub struct DefaultBidAdjustor;

impl BidAdjustor for DefaultBidAdjustor {
    fn try_apply_adjustments(
        &self,
        _bid: &PayloadEntry,
        _bid_adjustments_data: &Option<BidAdjustmentData>,
    ) -> Option<PayloadEntry> {
        None
    }

    fn on_new_slot(&mut self, _bid_slot: u64) {}
}
