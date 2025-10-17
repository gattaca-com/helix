use alloy_primitives::{Address, B256};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct InclusionList {
    pub txs: Vec<InclusionListTx>,
}

impl InclusionList {
    pub const fn _empty() -> Self {
        Self { txs: vec![] }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct InclusionListTx {
    pub hash: B256,
    pub nonce: u64,
    pub sender: Address,
    pub gas_priority_fee: u64,
    pub bytes: Bytes,
    pub wait_time: u32,
}
