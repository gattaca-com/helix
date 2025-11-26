use std::sync::Arc;

use alloy_primitives::B256;
use helix_types::{BlsKeypair, BlsPublicKeyBytes, BlsSignature, SignedRoot};

use crate::chain_info::ChainInfo;

// Arbitrary domain for signing relay messages
pub const RELAY_DOMAIN: &[u8; 32] = b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0relay";

#[derive(Clone)]
pub struct RelaySigningContext {
    pub keypair: BlsKeypair,
    pub pubkey: BlsPublicKeyBytes,
    pub context: Arc<ChainInfo>,
}

impl RelaySigningContext {
    pub fn new(keypair: BlsKeypair, context: Arc<ChainInfo>) -> Self {
        Self { pubkey: keypair.pk.serialize().into(), keypair, context }
    }

    pub fn pubkey(&self) -> &BlsPublicKeyBytes {
        &self.pubkey
    }

    pub fn sign_builder_message(&self, msg: &impl SignedRoot) -> BlsSignature {
        let domain = self.context.builder_domain;
        let root = msg.signing_root(domain);
        self.sign(root)
    }

    pub fn sign_relay_message(&self, msg: &impl SignedRoot) -> BlsSignature {
        let root = msg.signing_root(RELAY_DOMAIN.into());
        self.sign(root)
    }

    pub fn sign(&self, message: B256) -> BlsSignature {
        self.keypair.sk.sign(message)
    }
}

impl Default for RelaySigningContext {
    fn default() -> Self {
        Self {
            keypair: BlsKeypair::random(),
            context: ChainInfo::default().into(),
            pubkey: BlsPublicKeyBytes::default(),
        }
    }
}
