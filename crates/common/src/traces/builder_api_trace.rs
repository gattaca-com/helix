use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmissionTrace {
    pub receive: u64,
    pub decode: u64,
    pub floor_bid_checks: u64,
    pub pre_checks: u64,
    pub signature: u64,
    pub simulation: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
    pub metadata: Option<String>,
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
