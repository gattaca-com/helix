// use ethereum_consensus::altair::Bytes32;
// use ethereum_consensus::ssz::prelude::*;
// use crate::constraints::basic_tx_constraint::BasicTransactionConstraint;
// use crate::constraints::bolt::BoltConstraint;

// pub mod bolt;
// pub mod basic_tx_constraint;

// pub const MAX_CONSTRAINTS: usize = 4;

// #[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
// #[ssz(transparent)]
// #[serde(untagged)]
// pub enum Constraint {
//     BoltConstraint(BoltConstraint),
//     BasicTransactionConstraint(BasicTransactionConstraint),
// }

// impl Constraint {
//     /// Verifies that the constraint is valid for an ordered list of hashes.
//     ///
//     /// Returns true if the constraint is valid, false if not.
//     pub fn verify_from_tx_hash_vec(&self, tx_hashes: &Vec<Bytes32>) -> bool {
//         match self {
//             Constraint::BoltConstraint(constraint) => {
//                 constraint.verify_from_tx_hash_vec(tx_hashes)
//             }
//             Constraint::BasicTransactionConstraint(constraint) => {
//                 constraint.verify_from_tx_hash_vec(tx_hashes)
//             }
//         }
//     }
// }

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn test_serialise() {
//         let basic_tx_constraint = BasicTransactionConstraint {
//             hash: Default::default(),
//             transaction_bytes: Default::default(),
//         };
//         let constraint = Constraint::BasicTransactionConstraint(basic_tx_constraint);
//         let json_serialised = serde_json::to_string_pretty(&constraint);
//         println!("{:?}", json_serialised);
//     }
// }
