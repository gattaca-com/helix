use ethereum_consensus::{
    deneb::verify_signature,
    primitives::{BlsPublicKey, BlsSignature, Hash32, Slot},
    serde::as_str,
    ssz::prelude::*,
};
use helix_utils::signing::compute_builder_signing_root;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct Cancellation {
    #[serde(with = "as_str")]
    pub slot: Slot,
    pub parent_hash: Hash32,
    #[serde(rename = "builder_pubkey")]
    pub builder_public_key: BlsPublicKey,
    #[serde(rename = "proposer_pubkey")]
    pub proposer_public_key: BlsPublicKey,
}

#[derive(Clone, Debug, Default, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedCancellation {
    pub message: Cancellation,
    pub signature: BlsSignature,
}

impl SignedCancellation {
    pub fn new(message: Cancellation, signature: BlsSignature) -> Self {
        Self { message, signature }
    }

    pub fn verify_signature(
        &mut self,
        context: &ethereum_consensus::state_transition::Context,
    ) -> Result<(), ethereum_consensus::Error> {
        let mut msg = self.message.clone();
        let signing_root = compute_builder_signing_root(&mut msg, context)?;
        let public_key = &self.message.builder_public_key;
        verify_signature(public_key, signing_root.as_ref(), &self.signature)
    }
}
