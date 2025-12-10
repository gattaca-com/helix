use crate::auctioneer::PayloadEntry;

pub trait BidAdjustor: Send + Sync + 'static {
    fn try_apply_adjustments(&self, bid: &PayloadEntry) -> Option<PayloadEntry>;

    fn on_new_slot(&mut self, bid_slot: u64);
}

pub struct DefaultBidAdjustor;

impl BidAdjustor for DefaultBidAdjustor {
    fn try_apply_adjustments(&self, _bid: &PayloadEntry) -> Option<PayloadEntry> {
        None
    }

    fn on_new_slot(&mut self, _bid_slot: u64) {}
}
