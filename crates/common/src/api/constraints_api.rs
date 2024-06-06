use std::fmt::Debug;
use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, Hash32, Slot, U256, BlsSignature},
    ssz::prelude::*,
};
use ethereum_consensus::bellatrix::Transaction;
use ethereum_consensus::primitives::Bytes32;
use crate::constraints::Constraint;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedGatewayElection {
    pub message: GatewayElection,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedGatewayElection {
    pub fn gateway_public_key(&self) -> &BlsPublicKey {
        &self.message.gateway_public_key
    }

    pub fn slot(&self) -> u64 {
        self.message.slot
    }

    pub fn public_key(&self) -> &BlsPublicKey {
        &self.message.public_key
    }

    pub fn validator_index(&self) -> usize {
        self.message.validator_index
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GatewayElection {
    /// Public key of the gateway the proposer is delegating preconf control to.
    pub gateway_public_key: BlsPublicKey,
    /// Slot this delegation is valid for.
    pub slot: u64,
    /// Public key of the validator electing the gateway.
    pub public_key: BlsPublicKey,
    /// Validator index of the validator electing the gateway.
    pub validator_index: usize,
    /// Gateway address builder must pay
    pub gateway_recipient_address: ExecutionAddress,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetGatewayParams {
    /// The slot to get the elected gateway for.
    pub slot: u64,
}

#[derive(Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
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

    pub fn public_key(&self) -> &BlsPublicKey {
        &self.message.public_key
    }
}

#[derive(Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ConstraintsMessage {
    /// Slot these constraints are valid for.
    pub slot: u64,
    /// Public key of the gateway (or proposer) that is setting the constraints.
    pub public_key: BlsPublicKey,
    pub constraints: List<Constraint, 4>,  // TODO: set const
}