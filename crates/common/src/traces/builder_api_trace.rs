#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct SubmissionTrace {
    pub receive: u64,
    pub decode: u64,
    pub pre_checks: u64,
    pub signature: u64,
    pub floor_bid_checks: u64,
    pub simulation: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
}
