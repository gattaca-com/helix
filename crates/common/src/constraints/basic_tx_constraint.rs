use ethereum_consensus::bellatrix::mainnet::Transaction;
use ethereum_consensus::primitives::Bytes32;
use ethereum_consensus::ssz::prelude::*;


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BasicTransactionConstraint {
    pub hash: Bytes32,
    pub transaction_bytes: Transaction,
}