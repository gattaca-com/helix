use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::{
    bid_submission::OptimisticVersion, metrics::SUB_TRACE_LATENCY, utils::utcnow_ns,
    MiddlewareTimings,
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
    // when handler started
    pub start_handler: u64,
    pub decode: u64,
    pub floor_bid_checks: u64,
    pub pre_checks: u64,
    pub signature: u64,
    pub simulation: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
    pub metadata: Option<String>,

    pub skip_sigverify: bool,
}

impl SubmissionTrace {
    pub fn init_from_timings(timings: MiddlewareTimings) -> Self {
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
        if matches!(optimistic_version, OptimisticVersion::V2 | OptimisticVersion::V3) {
            // ignore v2/v3 as the traces come from the payload only
            return;
        }

        record("scheduled_at", self.receive, self.scheduled_at);
        record("read_body", self.scheduled_at, self.read_body);
        record("start_handler", self.read_body, self.start_handler);
        record("decode", self.start_handler, self.decode);
        record("floor_bid_checks", self.decode, self.floor_bid_checks);
        record("pre_checks", self.floor_bid_checks, self.pre_checks);

        if !self.skip_sigverify {
            record("signature", self.pre_checks, self.signature);
        }

        if optimistic_version.is_optimistic() {
            record("sim_optimistic", self.signature, self.simulation);
        } else {
            record("sim_non_optimistic", self.signature, self.simulation);
        }

        record("auctioneer_update", self.simulation, self.auctioneer_update);
        record("finish", self.auctioneer_update, self.request_finish);
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
    pub floor_bid_checks: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
    pub metadata: Option<String>,
}

impl HeaderSubmissionTrace {
    pub fn init_from_timings(timings: MiddlewareTimings) -> Self {
        Self { receive: timings.on_receive_ns, ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GossipedPayloadTrace {
    pub receive: u64,
    pub pre_checks: u64,
    pub auctioneer_update: u64,
}
