use alloy_primitives::B256;
use helix_types::{BlsPublicKey, BlsSignature, ChainSpec, SigError, SignedRoot, Slot};
use tree_hash_derive::TreeHash;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, TreeHash)]
pub struct Cancellation {
    pub slot: Slot,
    pub parent_hash: B256,
    #[serde(rename = "builder_pubkey")]
    pub builder_public_key: BlsPublicKey,
    #[serde(rename = "proposer_pubkey")]
    pub proposer_public_key: BlsPublicKey,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SignedCancellation {
    pub message: Cancellation,
    pub signature: BlsSignature,
}

impl SignedCancellation {
    pub fn new(message: Cancellation, signature: BlsSignature) -> Self {
        Self { message, signature }
    }

    pub fn verify_signature(&self, spec: &ChainSpec) -> Result<(), SigError> {
        let domain = spec.get_builder_domain();
        let message = self.message.signing_root(domain);
        let valid = self.signature.verify(&self.message.builder_public_key, message);

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }
}
