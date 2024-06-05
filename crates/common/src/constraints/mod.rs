use ethereum_consensus::ssz::prelude::*;
use crate::constraints::basic_tx_constraint::BasicTransactionConstraint;

pub mod bolt;
pub mod basic_tx_constraint;

pub const MAX_CONSTRAINTS: usize = 4;

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
#[ssz(transparent)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Constraint {
    BasicTransactionConstraint(BasicTransactionConstraint),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialise() {
        let basic_tx_constraint = BasicTransactionConstraint {
            hash: Default::default(),
            transaction_bytes: Default::default(),
        };
        let constraint = Constraint::BasicTransactionConstraint(basic_tx_constraint);
        let json_serialised = serde_json::to_string_pretty(&constraint);
        println!("{:?}", json_serialised);
    }
}