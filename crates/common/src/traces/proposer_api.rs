use serde::{Deserialize, Serialize};

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
    pub receive: u64,
    pub proposer_index_validated: u64,
    pub signature_validated: u64,
    pub payload_fetched: u64,
    pub validation_complete: u64,
    pub beacon_client_broadcast: u64,
    pub broadcaster_block_broadcast: u64,
    pub on_deliver_payload: u64,
}
