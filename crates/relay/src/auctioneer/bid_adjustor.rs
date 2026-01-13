use crate::auctioneer::{PayloadEntry, SimulatorRequest, types::SlotData};

pub trait BidAdjustor: Send + Sync + 'static {
    fn try_apply_adjustments(
        &self,
        bid: &PayloadEntry,
        slot_data: &SlotData,
        is_dry_run: bool,
    ) -> Option<(PayloadEntry, SimulatorRequest, bool)>;

    fn on_new_slot(&mut self, bid_slot: u64);
}

pub struct DefaultBidAdjustor;

impl BidAdjustor for DefaultBidAdjustor {
    fn try_apply_adjustments(
        &self,
        _bid: &PayloadEntry,
        _slot_data: &SlotData,
        _is_dry_run: bool,
    ) -> Option<(PayloadEntry, SimulatorRequest, bool)> {
        None
    }

    fn on_new_slot(&mut self, _bid_slot: u64) {}
}
