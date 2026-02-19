use std::{sync::atomic::Ordering, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{RequestTimings, metrics::SUB_TRACE_LATENCY, utils::utcnow_ns};

// all timestamps are in nanoseconds
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmissionTrace {
    // first packet
    pub receive_ns: u64,
    // when body finished being read
    pub read_body_ns: u64,
    // when body finished being decoded
    pub decoded_ns: u64,
    pub metadata: Option<String>,
}

impl SubmissionTrace {
    pub fn init_from_timings(timings: RequestTimings) -> Self {
        let read_body = timings.stats.finish_ns.load(Ordering::Relaxed);
        record_submission_step_ns("start_handler", read_body, utcnow_ns());
        Self { receive_ns: timings.on_receive_ns, read_body_ns: read_body, ..Default::default() }
    }
}

pub fn record_submission_step(label: &str, duration: Duration) {
    let value = duration.as_nanos() as f64 / 1000.;
    SUB_TRACE_LATENCY.with_label_values(&[label]).observe(value);
}

pub fn record_submission_step_ns(label: &str, start: u64, end: u64) {
    if end > start {
        let value = (end - start) as f64 / 1000.;
        SUB_TRACE_LATENCY.with_label_values(&[label]).observe(value);
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GossipedPayloadTrace {
    pub receive: u64,
    pub pre_checks: u64,
    pub auctioneer_update: u64,
}
