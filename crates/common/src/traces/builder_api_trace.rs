use std::sync::atomic::Ordering;

use flux::{timing::Nanos, type_hash_derive::type_hash_lock};
use flux_utils::ArrayStr;
use flux_versioned_types::versioned_struct;
use serde::{Deserialize, Serialize};

use crate::{RequestTimings, metrics::SUB_TRACE_LATENCY, utils::utcnow_ns};

versioned_struct!(SubmissionTrace =>
    #[derive(Default)]
    #[type_hash_lock(hash = 11267690528330172066)]
    SubmissionTraceV1 {
    // first packet
    pub receive_ns: Nanos,
    // when body finished being read
    pub read_body_ns: Nanos,
    // when body finished being decoded
    pub decoded_ns: Nanos,
    /// Empty when no metadata was provided. Plain `ArrayStr` rather than
    /// `Option<ArrayStr<128>>` because this type crosses the spine's
    /// extern "C" queues, and `Option` has no guaranteed layout.
    pub metadata: ArrayStr<128>,
    }
);

impl SubmissionTrace {
    pub fn init_from_timings(timings: RequestTimings) -> Self {
        let read_body = timings.stats.finish_ns.load(Ordering::Relaxed);
        record_submission_step_ns("start_handler", read_body, utcnow_ns());
        Self {
            receive_ns: Nanos(timings.on_receive_ns),
            read_body_ns: Nanos(read_body),
            ..Default::default()
        }
    }
}

pub fn record_submission_step(label: &str, duration: Nanos) {
    let value = duration.0 as f64 / 1000.;
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
