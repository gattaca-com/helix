use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::{metrics::GET_PAYLOAD_TRACE_LATENCY, utils::utcnow_ns, RequestTimings};

#[derive(Debug, Clone, Copy, Default)]
pub struct RegisterValidatorsTrace {
    pub receive: u64,
    pub registrations_complete: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GetHeaderTrace {
    pub receive: u64,
    pub validation_complete: u64,
    pub best_bid_fetched: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GetPayloadTrace {
    // first packet
    pub receive: u64,
    // when body started being read
    pub scheduled_at: u64,
    // when body finished being read
    pub read_body: u64,
    // when handler started
    pub start_handler: u64,
    // when payload was decoded
    pub decode: u64,
    pub proposer_index_validated: u64,
    pub signature_validated: u64,
    pub payload_fetched: u64,
    pub validation_complete: u64,
    pub beacon_client_broadcast: u64,
    pub broadcaster_block_broadcast: u64,
    pub on_deliver_payload: u64,
}

impl GetPayloadTrace {
    pub fn init_from_timings(timings: RequestTimings) -> Self {
        let scheduled_at = timings.stats.start_ns.load(Ordering::Relaxed);
        let read_body = timings.stats.finish_ns.load(Ordering::Relaxed);

        Self {
            receive: timings.on_receive_ns,
            scheduled_at,
            read_body,
            start_handler: utcnow_ns(),
            ..Default::default()
        }
    }

    pub fn record_metrics(&self) {
        record("scheduled_at", self.receive, self.scheduled_at);
        record("read_body", self.scheduled_at, self.read_body);
        record("start_handler", self.read_body, self.start_handler);
        record("decode", self.start_handler, self.decode);
        record("proposer_index_validated", self.decode, self.proposer_index_validated);
        record("signature_validated", self.proposer_index_validated, self.signature_validated);
        record("payload_fetched", self.signature_validated, self.payload_fetched);
        record("validation_complete", self.payload_fetched, self.validation_complete);
        record("beacon_client_broadcast", self.validation_complete, self.beacon_client_broadcast);
        record(
            "broadcaster_block_broadcast",
            self.beacon_client_broadcast,
            self.broadcaster_block_broadcast,
        );
        record("on_deliver_payload", self.broadcaster_block_broadcast, self.on_deliver_payload);
    }
}

fn record(label: &str, start: u64, end: u64) {
    if end > start {
        let value = (end - start) as f64 / 1000.;
        GET_PAYLOAD_TRACE_LATENCY.with_label_values(&[label]).observe(value);
    }
}
