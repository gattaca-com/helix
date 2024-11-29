#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConstraintSubmissionTrace {
    pub receive: u64,
    pub decode: u64,
    pub verify_signature: u64,
    pub auctioneer_update: u64,
    pub request_finish: u64,
}
