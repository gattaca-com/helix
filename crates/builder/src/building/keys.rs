use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use helix_types::{BlsKeypair, BlsPublicKeyBytes, BlsSecretKey};

/// BLS secret that signs the submission. Distinct from the merging role's
/// `RELAY_KEY`, which is a secp256k1 secret.
pub const BUILDER_BLS_KEY_ENV: &str = "BUILDER_BLS_KEY";
/// secp256k1 secret that signs the payout to the proposer. Must be funded.
pub const BUILDER_PAYOUT_KEY_ENV: &str = "BUILDER_PAYOUT_KEY";

#[derive(Debug)]
pub struct BuildingKeys {
    pub bls: BlsKeypair,
    pub payout: PrivateKeySigner,
}

impl BuildingKeys {
    pub fn load() -> eyre::Result<Self> {
        let bls = std::env::var(BUILDER_BLS_KEY_ENV)
            .map_err(|_| eyre::eyre!("{BUILDER_BLS_KEY_ENV} env var not set"))?;
        let payout = std::env::var(BUILDER_PAYOUT_KEY_ENV)
            .map_err(|_| eyre::eyre!("{BUILDER_PAYOUT_KEY_ENV} env var not set"))?;
        Self::parse(&bls, &payout)
    }

    /// Split from [`Self::load`] so tests never touch process-wide env vars.
    pub fn parse(bls_hex: &str, payout_hex: &str) -> eyre::Result<Self> {
        let bytes = hex::decode(bls_hex.trim().trim_start_matches("0x"))
            .map_err(|e| eyre::eyre!("invalid {BUILDER_BLS_KEY_ENV}: {e}"))?;
        let secret = BlsSecretKey::deserialize(&bytes)
            .map_err(|e| eyre::eyre!("invalid {BUILDER_BLS_KEY_ENV}: {e:?}"))?;
        let bls = BlsKeypair::from_components(secret.public_key(), secret);

        let payout: PrivateKeySigner = payout_hex
            .trim()
            .parse()
            .map_err(|e| eyre::eyre!("invalid {BUILDER_PAYOUT_KEY_ENV}: {e}"))?;

        Ok(Self { bls, payout })
    }

    pub fn pubkey(&self) -> BlsPublicKeyBytes {
        self.bls.pk.serialize().into()
    }

    pub fn payout_address(&self) -> Address {
        self.payout.address()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Non-zero, canonical-range scalars.
    const BLS_HEX: &str = "3535353535353535353535353535353535353535353535353535353535353535";
    const PAYOUT_HEX: &str = "4646464646464646464646464646464646464646464646464646464646464646";

    #[test]
    fn parses_a_hex_keypair() {
        let bare = BuildingKeys::parse(BLS_HEX, PAYOUT_HEX).expect("bare hex must parse");
        let prefixed = BuildingKeys::parse(&format!("0x{BLS_HEX}"), &format!("0x{PAYOUT_HEX}"))
            .expect("0x-prefixed hex must parse");

        assert_eq!(bare.pubkey(), prefixed.pubkey());
        assert_eq!(bare.payout_address(), prefixed.payout_address());
    }

    #[test]
    fn rejects_a_malformed_bls_key() {
        let err = BuildingKeys::parse("nothex", PAYOUT_HEX).expect_err("must not start");

        assert!(err.to_string().contains(BUILDER_BLS_KEY_ENV), "got: {err}");
    }

    #[test]
    fn rejects_a_malformed_payout_key() {
        let err = BuildingKeys::parse(BLS_HEX, "nothex").expect_err("must not start");

        assert!(err.to_string().contains(BUILDER_PAYOUT_KEY_ENV), "got: {err}");
    }

    #[test]
    fn derives_the_pubkey_and_payout_address() {
        let keys = BuildingKeys::parse(BLS_HEX, PAYOUT_HEX).unwrap();

        // Both are logged at boot: the operator registers one and funds the other.
        assert_ne!(keys.pubkey(), BlsPublicKeyBytes::default());
        assert_ne!(keys.payout_address(), Address::ZERO);
    }
}
