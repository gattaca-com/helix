use std::time::Instant;

use ethereum_consensus::primitives::U256;

#[derive(Debug, Clone)]
pub struct SaveBidAndUpdateTopBidResponse {
    pub was_bid_saved: bool,
    pub was_top_bid_updated: bool,
    pub is_new_top_bid: bool,

    pub top_bid_value: U256,
    pub prev_top_bid_value: U256,

    pub latency_get_prev_top_bid: u128,
    pub latency_save_payload: u128,
    pub latency_save_bid: u128,
    pub latency_save_trace: u128,
    pub latency_update_top_bid: u128,
    pub latency_update_floor: u128,

    pub last_recorded_time: Instant,
}

impl Default for SaveBidAndUpdateTopBidResponse {
    fn default() -> Self {
        Self::new()
    }
}

impl SaveBidAndUpdateTopBidResponse {
    pub fn new() -> Self {
        Self {
            was_bid_saved: false,
            was_top_bid_updated: false,
            is_new_top_bid: false,

            top_bid_value: U256::ZERO,
            prev_top_bid_value: U256::ZERO,

            latency_get_prev_top_bid: 0,
            latency_save_payload: 0,
            latency_save_bid: 0,
            latency_save_trace: 0,
            latency_update_top_bid: 0,
            latency_update_floor: 0,

            last_recorded_time: Instant::now(),
        }
    }

    pub fn set_latency_get_prev_top_bid(&mut self) {
        self.latency_get_prev_top_bid = self.record_time();
    }

    pub fn set_latency_save_payload(&mut self) {
        self.latency_save_payload = self.record_time();
    }

    pub fn set_latency_save_bid(&mut self) {
        self.latency_save_bid = self.record_time();
    }

    pub fn set_latency_save_trace(&mut self) {
        self.latency_save_trace = self.record_time();
    }

    pub fn set_latency_update_top_bid(&mut self) {
        self.latency_update_top_bid = self.record_time();
    }

    pub fn set_latency_update_floor(&mut self) {
        self.latency_update_floor = self.record_time();
    }

    fn record_time(&mut self) -> u128 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_recorded_time).as_nanos();
        self.last_recorded_time = now;
        elapsed
    }
}
