use ethereum_consensus::{
    primitives::{BlsPublicKey, BlsSignature},
    ssz::prelude::*,
};
use sha2::{Digest, Sha256};

pub const MAX_CONSTRAINTS_PER_SLOT: usize = 256;

/// The action type for a delegation message.
pub const DELEGATION_ACTION: u8 = 0;

/// The action type for a revocation message.
pub const REVOCATION_ACTION: u8 = 1;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Hash, PartialEq, Eq)]
pub struct SignedDelegation {
    pub message: DelegationMessage,
    pub signature: BlsSignature,
}

#[derive(
    Debug, Clone, SimpleSerialize, serde::Deserialize, serde::Serialize, Hash, PartialEq, Eq,
)]
pub struct DelegationMessage {
    pub action: u8,
    pub validator_pubkey: BlsPublicKey,
    pub delegatee_pubkey: BlsPublicKey,
}

impl SignableBLS for DelegationMessage {
    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update([self.action]);
        hasher.update(&self.validator_pubkey.to_vec());
        hasher.update(&self.delegatee_pubkey.to_vec());

        hasher.finalize().into()
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SignedRevocation {
    pub message: RevocationMessage,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Deserialize, serde::Serialize)]
pub struct RevocationMessage {
    pub action: u8,
    pub validator_pubkey: BlsPublicKey,
    pub delegatee_pubkey: BlsPublicKey,
}

impl SignableBLS for RevocationMessage {
    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update([self.action]);
        hasher.update(&self.validator_pubkey.to_vec());
        hasher.update(&self.delegatee_pubkey.to_vec());

        hasher.finalize().into()
    }
}

/// Trait for any types that can be signed and verified with BLS.
/// This trait is used to abstract over the signing and verification of different types.
pub trait SignableBLS {
    /// Returns the digest of the object.
    fn digest(&self) -> [u8; 32];
}
