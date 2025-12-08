use helix_types::BidAdjustmentData;

use crate::auctioneer::PayloadHeaderData;

pub trait BidAdjustor: Send + Sync + Clone + 'static {
    fn try_apply_adjustments(
        &self,
        bid: &PayloadHeaderData,
        bid_adjustments_data: &Option<BidAdjustmentData>,
    ) -> Option<PayloadHeaderData>;
}

#[derive(Clone)]
pub struct DefaultBidAdjustor;

impl BidAdjustor for DefaultBidAdjustor {
    fn try_apply_adjustments(
        &self,
        _bid: &PayloadHeaderData,
        _bid_adjustments_data: &Option<BidAdjustmentData>,
    ) -> Option<PayloadHeaderData> {
        None
    }
}
