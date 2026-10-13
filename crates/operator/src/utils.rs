use libp2p::identity::{
    DecodingError, Keypair,
    secp256k1::{self, SecretKey},
};

pub fn load_operator_keypair() -> Keypair {
    let operator_key_str = std::env::var("OPERATOR_KEY").expect("could not load OPERATOR_KEY");
    let mut operator_key_bytes =
        alloy_primitives::hex::decode(operator_key_str).expect("invalid OPERATOR_KEY bytes");
    keypair_from_bytes(&mut operator_key_bytes).expect("failed to decode operator key bytes")
}

pub fn keypair_from_bytes(bytes: &mut [u8]) -> Result<Keypair, DecodingError> {
    let secret_key = SecretKey::try_from_bytes(bytes)?;
    Ok(secp256k1::Keypair::from(secret_key).into())
}

#[cfg(test)]
mod tests {
    use helix_common::OperatorConfig;
    use libp2p::identity::Keypair;

    #[test]
    fn keygen() {
        let keypair = Keypair::generate_secp256k1();
        let s_pair = keypair.try_into_secp256k1().unwrap();
        let secret_key_bytes = s_pair.secret().to_bytes();
        let public_key_bytes = s_pair.public().to_bytes();
        println!("private: {}", hex::encode(secret_key_bytes));
        println!("public: {}", hex::encode(public_key_bytes));
    }

    #[test]
    fn load_config() {
        let yaml = std::fs::read_to_string("config/production.yml").unwrap();
        let config: OperatorConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(3, config.operators.len());
    }
}
