use helix_types::BidAdjustmentData;

use crate::auctioneer::PayloadEntry;

pub trait BidAdjustor: Send + Sync + Clone + 'static {
    fn try_apply_adjustments(
        &self,
        bid: &PayloadEntry,
        bid_adjustments_data: &Option<BidAdjustmentData>,
    ) -> Option<PayloadEntry>;
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
}
