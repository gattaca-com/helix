use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::{RequestTimings, metrics::GET_PAYLOAD_TRACE_LATENCY, utils::utcnow_ns};

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
    // when body finished being read
    pub read_body: u64,
    // when handler started
    pub start_handler: u64,
    // when payload was decoded
    pub decode: u64,
    pub proposer_index_validated: u64,
    pub signature_validated: u64,
    pub payload_fetched: u64,
    #[serde(default)]
    pub slot_start_awaited: u64,
    /// After `gossip_payload` returns: SSZ encode, one deep clone + gzip per peer
    /// relay, and the operator broadcast. Awaited on both API versions, so without
    /// this stamp it is indistinguishable from the beacon publish that follows it.
    #[serde(default)]
    pub gossiped: u64,
    pub validation_complete: u64,
    pub beacon_client_broadcast: u64,
    pub broadcaster_block_broadcast: u64,
    pub on_deliver_payload: u64,
}

impl GetPayloadTrace {
    pub fn init_from_timings(timings: RequestTimings) -> Self {
        let read_body = timings.stats.finish_ns.load(Ordering::Relaxed);

        Self {
            receive: timings.on_receive_ns,
            read_body,
            start_handler: utcnow_ns(),
            ..Default::default()
        }
    }

    pub fn record_metrics(&self) {
        let steps = [
            ("read_body", self.read_body),
            ("start_handler", self.start_handler),
            ("decode", self.decode),
            ("signature_validated", self.signature_validated),
            ("proposer_index_validated", self.proposer_index_validated),
            ("payload_fetched", self.payload_fetched),
            ("slot_start_awaited", self.slot_start_awaited),
            ("gossiped", self.gossiped),
            ("validation_complete", self.validation_complete),
            ("beacon_client_broadcast", self.beacon_client_broadcast),
            ("broadcaster_block_broadcast", self.broadcaster_block_broadcast),
            ("on_deliver_payload", self.on_deliver_payload),
        ];

        let mut prev = self.receive;
        for (label, stamp) in steps {
            if stamp == 0 {
                continue;
            }
            record(label, prev, stamp);
            prev = stamp;
        }
    }
}

fn record(label: &str, start: u64, end: u64) {
    // `start == 0` means the preceding step was never stamped; without this guard the
    // subtraction degenerates into the absolute timestamp.
    if start > 0 && end > start {
        let value = (end - start) as f64 / 1000.;
        GET_PAYLOAD_TRACE_LATENCY.with_label_values(&[label]).observe(value);
    }
}
