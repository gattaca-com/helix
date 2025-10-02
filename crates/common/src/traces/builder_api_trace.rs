use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::{
    bid_submission::OptimisticVersion, metrics::SUB_TRACE_LATENCY, utils::utcnow_ns, RequestTimings,
};

// all timestamps are in nanoseconds
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmissionTrace {
    // first packet
    pub receive: u64,
    // when body started being read
    pub scheduled_at: u64,
    // when body finished being read
    pub read_body: u64,
    // when tokio request handler started
    pub start_handler: u64,
    // when handler sent to worker
    pub sent_worker: u64,
    /// when worker received
    pub received_worker: u64,
    pub decode: u64,
    /// None if skip_sigverify
    pub signature: Option<u64>,
    pub validated: u64,
    /// Can either before (optimistic) or after simulation
    pub sorted: u64,
    pub simulation: u64,
    pub metadata: Option<String>,
    /// When a _newly processed submission_ triggered a top bid update (new top bid or cancel)
    /// ts in nanos, is_cancel
    pub top_bid_at: Option<(u64, bool)>,
}

impl SubmissionTrace {
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

    pub fn record_metrics(&self, optimistic_version: OptimisticVersion) {
        if matches!(optimistic_version, OptimisticVersion::V3) {
            // ignore v3 as the traces come from the payload only
            return;
        }

        record("scheduled_at", self.receive, self.scheduled_at);
        record("read_body", self.scheduled_at, self.read_body);
        record("start_handler", self.read_body, self.start_handler);
        record("sent_worker", self.start_handler, self.sent_worker);
        record("received_worker", self.sent_worker, self.received_worker);
        record("decode", self.received_worker, self.decode);
        if let Some(signature) = self.signature {
            record("signature", self.decode, signature);
            record("validated", signature, self.validated);
        } else {
            record("validated", self.decode, self.validated);
        }

        if self.sorted < self.simulation {
            // optimistic
            record("sorted", self.validated, self.sorted);
            record("simulation", self.sorted, self.simulation);
        } else {
            // non optimistic
            record("simulation", self.validated, self.simulation);
            record("sorted", self.simulation, self.sorted);
        }

        if let Some((top_bid_at, is_cancel)) = self.top_bid_at {
            if is_cancel {
                // this is our "tick to trade" but may be confounded if builder is sending slowly
                record("recv_top_bid_cancel", self.receive, top_bid_at);
                // internal overhead
                record("read_body_top_bid_cancel", self.read_body, top_bid_at);
            } else {
                record("recv_top_bid", self.receive, top_bid_at);
                record("read_body_top_bid", self.read_body, top_bid_at);
            }
        }
    }
}

fn record(label: &str, start: u64, end: u64) {
    if end > start {
        let value = (end - start) as f64 / 1000.;
        SUB_TRACE_LATENCY.with_label_values(&[label]).observe(value);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeaderSubmissionTrace {
    pub receive: u64,
    pub decode: u64,
    pub pre_checks: u64,
    pub signature: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
    pub metadata: Option<String>,
}

impl HeaderSubmissionTrace {
    pub fn init_from_timings(timings: RequestTimings) -> Self {
        Self { receive: timings.on_receive_ns, ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GossipedPayloadTrace {
    pub receive: u64,
    pub pre_checks: u64,
    pub auctioneer_update: u64,
}
