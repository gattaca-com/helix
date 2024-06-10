use ethereum_consensus::bellatrix::mainnet::Transaction;
use ethereum_consensus::primitives::Bytes32;
use ethereum_consensus::ssz::prelude::*;


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BasicTransactionConstraint {
    pub hash: Bytes32,
    pub transaction_bytes: Transaction,
}

impl BasicTransactionConstraint {
    /// Verifies that the constraint is valid for an ordered list of hashes.
    ///
    /// Returns true if the constraint is valid, false if not.
    pub fn verify_from_tx_hash_vec(&self, tx_hashes: &Vec<Bytes32>) -> bool {
        tx_hashes.contains(&self.hash)
    }
}