use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, Hash32, Slot, U256, BlsSignature},
    ssz::prelude::*,
};

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedGatewayElection {
    pub message: GatewayElection,
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
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GatewayElection {
    pub gateway_public_key: BlsPublicKey,
    pub slot: u64,
    pub public_key: BlsPublicKey,
}