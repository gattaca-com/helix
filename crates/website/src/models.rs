use deadpool_postgres::tokio_postgres::{Error, Row};
use helix_common::bid_submission::BidTrace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct NumRegisteredValidators {
    pub num_validators: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeliveredPayload {
    pub bid_trace: BidTrace,
    pub block_number: u64,
    pub epoch: u64,
    pub num_txs: usize,
}

impl DeliveredPayload {
    pub fn epoch(&self) -> u64 {
        self.bid_trace.slot / 32
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NumPayloads {
    pub num_payloads: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LatestSlot {
    pub slot: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkValidator {
    pub public_key: Vec<u8>,
    pub index: i64,
}

impl TryFrom<Row> for NetworkValidator {
    type Error = Error;

    fn try_from(row: Row) -> Result<Self, Self::Error> {
        Ok(Self { public_key: row.get("public_key"), index: row.get("index") })
    }
}
