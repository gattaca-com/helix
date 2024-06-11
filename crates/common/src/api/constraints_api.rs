use std::fmt::Debug;
use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, BlsSignature},
    ssz::prelude::*,
};
use crate::builder_api::BuilderGetValidatorsResponseEntry;
use crate::constraints::Constraint;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedPreconferElection {
    pub message: ElectedPreconfer,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedPreconferElection {
    pub fn gateway_info(&self) -> Option<&GatewayInfo> {
        self.message.gateway_info.as_ref()
    }

    pub fn slot(&self) -> u64 {
        self.message.slot
    }

    pub fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.message.proposer_public_key
    }

    pub fn validator_index(&self) -> usize {
        self.message.validator_index
    }
}

impl SignedPreconferElection {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            message: ElectedPreconfer::from_proposer_duty(duty),
            signature: Default::default(),
        }
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ElectedPreconfer {
    /// Slot this delegation is valid for.
    pub slot: u64,
    /// Public key of the validator proposing for `slot`.
    pub proposer_public_key: BlsPublicKey,
    /// Validator index of the validator proposing for `slot`.
    pub validator_index: usize,
    /// `None` if the proposer is handling the pre-confirmations.
    pub gateway_info: Option<GatewayInfo>,
}

impl ElectedPreconfer {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            slot: duty.slot,
            proposer_public_key: duty.entry.registration.message.public_key.clone(),
            validator_index: duty.validator_index,
            gateway_info: None,
        }
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GatewayInfo {
    /// Public key of the gateway the proposer is delegating pre-confirmation control to.
    pub gateway_public_key: BlsPublicKey,
    /// Gateway recipient address builder must pay.
    pub gateway_recipient_address: ExecutionAddress,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetGatewayParams {
    /// The slot to get the elected gateway for.
    pub slot: u64,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedConstraintsMessage {
    pub message: ConstraintsMessage,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedConstraintsMessage {
    pub fn slot(&self) -> u64 {
        self.message.slot
    }

    pub fn constraints(&self) -> &List<Constraint, 4> {
        &self.message.constraints
    }

    pub fn validator_index(&self) -> Option<u64> {
        self.message.validator_index
    }

    pub fn gateway_public_key(&self) -> Option<&BlsPublicKey> {
        self.message.gateway_public_key.as_ref()
    }
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ConstraintsMessage {
    /// Validator index that is setting the constraints.
    /// This will be `Some` if it is the proposer directly.
    pub validator_index: Option<u64>,
    /// Public key of the gateway that is setting the constraints.
    /// This will be `Some` if an elected gateway is setting the constraints.
    pub gateway_public_key: Option<BlsPublicKey>,
    /// Slot these constraints are valid for.
    pub slot: u64,
    pub constraints: List<Constraint, 4>,  // TODO: set const
}

#[cfg(test)]
mod tests {
    use reth_primitives::hex;
    use super::*;

    #[test]
    fn test_deserialise_signed_constraints() {
        let hex_str = "7b226d657373616765223a7b2276616c696461746f725f696e646578223a3132332c22676174657761795f7075626c69635f6b6579223a223078303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030222c22736c6f74223a313234352c22636f6e73747261696e7473223a5b7b227478223a2231373034303530363038303930303137303430343035343330383033222c22696e646578223a313233317d5d7d2c227369676e6174757265223a223078303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030227d";
        let raw_bytes: Vec<u8> = hex::decode(hex_str).unwrap();
        let res = serde_json::from_slice::<SignedConstraintsMessage>(&raw_bytes);
        println!("{res:?}");
    }
}