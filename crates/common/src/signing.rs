use std::sync::Arc;

use alloy_primitives::B256;
use helix_types::{BlsKeypair, BlsPublicKey, BlsSignature, SignedRoot};

use crate::chain_info::ChainInfo;

#[derive(Clone)]
pub struct RelaySigningContext {
    pub keypair: BlsKeypair,
    pub context: Arc<ChainInfo>,
}

impl RelaySigningContext {
    pub fn pubkey(&self) -> &BlsPublicKey {
        &self.keypair.pk
    }

    pub fn sign_builder_message(&self, msg: &impl SignedRoot) -> BlsSignature {
        let domain = self.context.context.get_builder_domain();
        let root = msg.signing_root(domain);
        self.sign(root)
    }

    pub fn sign(&self, message: B256) -> BlsSignature {
        self.keypair.sk.sign(message)
    }
}

impl Default for RelaySigningContext {
    fn default() -> Self {
        Self { keypair: BlsKeypair::random(), context: ChainInfo::for_mainnet().into() }
    }
}
