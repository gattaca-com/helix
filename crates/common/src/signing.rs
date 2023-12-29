use ethereum_consensus::{
    crypto::SecretKey,
    deneb::{BlsPublicKey, Context},
};


#[derive(Clone, Default)]
pub struct RelaySigningContext {
    pub public_key: BlsPublicKey,
    pub signing_key: SecretKey,
    pub context: Context,
}