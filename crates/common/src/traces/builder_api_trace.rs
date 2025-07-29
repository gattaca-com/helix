use serde::{Deserialize, Serialize};

use crate::metrics::SUB_TRACE_LATENCY;

// all timestamps are in nanoseconds
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmissionTrace {
    pub receive: u64,
    pub read_body: u64,
    pub decode: u64,
    pub floor_bid_checks: u64,
    pub pre_checks: u64,
    pub signature: u64,
    pub simulation: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
    pub is_optimistic: bool,
    pub metadata: Option<String>,
}

impl SubmissionTrace {
    pub fn record_metrics(&self) {
        let read_body = self.read_body.saturating_sub(self.receive) as f64 / 1000.;
        let decode = self.decode.saturating_sub(self.read_body) as f64 / 1000.;
        let floor_bid_checks = self.floor_bid_checks.saturating_sub(self.decode) as f64 / 1000.;
        let pre_checks = self.pre_checks.saturating_sub(self.floor_bid_checks) as f64 / 1000.;
        let signature = self.signature.saturating_sub(self.pre_checks) as f64 / 1000.;
        let simulation = self.simulation.saturating_sub(self.signature) as f64 / 1000.;
        let auctioneer_update =
            self.auctioneer_update.saturating_sub(self.simulation) as f64 / 1000.;
        let finish = self.request_finish.saturating_sub(self.auctioneer_update) as f64 / 1000.;

        SUB_TRACE_LATENCY.with_label_values(&["receive"]).observe(decode);
        SUB_TRACE_LATENCY.with_label_values(&["read_body"]).observe(read_body);
        SUB_TRACE_LATENCY.with_label_values(&["decode"]).observe(decode);
        SUB_TRACE_LATENCY.with_label_values(&["floor_bid_checks"]).observe(floor_bid_checks);
        SUB_TRACE_LATENCY.with_label_values(&["pre_checks"]).observe(pre_checks);
        SUB_TRACE_LATENCY.with_label_values(&["signature"]).observe(signature);
        if self.is_optimistic {
            SUB_TRACE_LATENCY.with_label_values(&["sim_optimistic"]).observe(simulation);
        } else {
            SUB_TRACE_LATENCY.with_label_values(&["sim_non_optimistic"]).observe(simulation);
        }
        SUB_TRACE_LATENCY.with_label_values(&["auctioneer_update"]).observe(auctioneer_update);
        SUB_TRACE_LATENCY.with_label_values(&["request_finish"]).observe(finish);
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GossipedPayloadTrace {
    pub receive: u64,
    pub pre_checks: u64,
    pub auctioneer_update: u64,
}
