use ethereum_consensus::bellatrix::mainnet::Transaction;
use ethereum_consensus::primitives::Bytes32;
use ethereum_consensus::ssz::prelude::*;


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BoltConstraint {
    pub tx: Transaction,
    pub index: Option<usize>,
}

impl BoltConstraint {
    /// Verifies that the constraint is valid for an ordered list of hashes.
    ///
    /// Returns true if the constraint is valid, false if not.
    pub fn verify_from_tx_hash_vec(&self, tx_hashes: &Vec<Bytes32>) -> bool {
        true  // TODO
    }
}

#[cfg(test)]
mod tests {
    use reth_primitives::hex;
    use super::*;

    #[test]
    fn test_deserialise() {
        let hex_str = "7b227478223a2231373034303530363038303930303137303430343035343330383033222c22696e646578223a313233317d";
        let res = serde_json::from_slice::<BoltConstraint>(&hex::decode(hex_str).unwrap());
        println!("{res:?}");
    }
}