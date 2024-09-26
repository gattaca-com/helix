use ethereum_consensus::{
    crypto::SecretKey,
    deneb::{BlsPublicKey, Context},
};

#[derive(Clone)]
pub struct RelaySigningContext {
    pub public_key: BlsPublicKey,
    pub signing_key: SecretKey,
    pub context: Context,
}

impl Default for RelaySigningContext {
    fn default() -> Self {
        Self { public_key: BlsPublicKey::default(), signing_key: SecretKey::default(), context: Context::for_minimal() }
    }
}
