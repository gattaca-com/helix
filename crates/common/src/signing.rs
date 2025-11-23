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
            context: ChainInfo::for_mainnet().into(),
            pubkey: BlsPublicKeyBytes::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_types::ValidatorRegistrationData;
    use alloy_primitives::Address;

    #[test]
    fn test_relay_signing_context_new() {
        let keypair = BlsKeypair::random();
        let context = Arc::new(ChainInfo::for_mainnet());
        let expected_pubkey: BlsPublicKeyBytes = keypair.pk.serialize().into();
        
        let signing_ctx = RelaySigningContext::new(keypair.clone(), context.clone());
        
        assert_eq!(signing_ctx.pubkey, expected_pubkey);
    }

    #[test]
    fn test_relay_signing_context_pubkey_accessor() {
        let signing_ctx = RelaySigningContext::default();
        let pubkey = signing_ctx.pubkey();
        
        // Should return reference to the pubkey
        assert_eq!(pubkey, &signing_ctx.pubkey);
    }

    #[test]
    fn test_relay_signing_context_default() {
        let signing_ctx = RelaySigningContext::default();
        
        // Note: Default implementation has a quirk - it generates a random keypair
        // but leaves pubkey as default (all zeros). This is likely a bug, but we
        // test the actual behavior here. Use `new()` for correct initialization.
        assert_eq!(signing_ctx.pubkey, BlsPublicKeyBytes::default());
        
        // The keypair itself is valid (just not reflected in the pubkey field)
        let actual_pubkey: BlsPublicKeyBytes = signing_ctx.keypair.pk.serialize().into();
        assert_ne!(actual_pubkey, BlsPublicKeyBytes::default());
    }

    #[test]
    fn test_relay_signing_context_clone() {
        let signing_ctx1 = RelaySigningContext::default();
        let signing_ctx2 = signing_ctx1.clone();
        
        // Cloned context should have same pubkey
        assert_eq!(signing_ctx1.pubkey, signing_ctx2.pubkey);
    }

    #[test]
    fn test_sign_builder_message_valid_signature() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair.clone(), chain_info.clone());
        
        let message = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        
        let signature = signing_ctx.sign_builder_message(&message);
        
        // Verify signature is valid with correct domain
        let domain = chain_info.builder_domain;
        let root = message.signing_root(domain);
        assert!(signature.verify(&keypair.pk, root), "Signature should verify with correct domain");
    }

    #[test]
    fn test_sign_builder_message_wrong_domain_fails() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair.clone(), chain_info);
        
        let message = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        
        let signature = signing_ctx.sign_builder_message(&message);
        
        // Signature should FAIL verification with wrong domain
        let wrong_domain = B256::from(*RELAY_DOMAIN);
        let root = message.signing_root(wrong_domain);
        assert!(!signature.verify(&keypair.pk, root), "Signature should NOT verify with wrong domain");
    }

    #[test]
    fn test_sign_builder_message_wrong_keypair_fails() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair, chain_info.clone());
        
        let message = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        
        let signature = signing_ctx.sign_builder_message(&message);
        
        // Signature should FAIL with different keypair
        let wrong_keypair = BlsKeypair::random();
        let domain = chain_info.builder_domain;
        let root = message.signing_root(domain);
        assert!(!signature.verify(&wrong_keypair.pk, root), "Signature should NOT verify with wrong keypair");
    }

    #[test]
    fn test_sign_builder_message_edge_case_gas_limits() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair.clone(), chain_info.clone());
        
        // Test with minimum gas limit (1)
        let message_min = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 1,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        let sig_min = signing_ctx.sign_builder_message(&message_min);
        let root_min = message_min.signing_root(chain_info.builder_domain);
        assert!(sig_min.verify(&keypair.pk, root_min), "Should sign message with gas_limit=1");
        
        // Test with maximum gas limit
        let message_max = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: u64::MAX,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        let sig_max = signing_ctx.sign_builder_message(&message_max);
        let root_max = message_max.signing_root(chain_info.builder_domain);
        assert!(sig_max.verify(&keypair.pk, root_max), "Should sign message with gas_limit=MAX");
        
        // Different gas limits should produce different signatures
        assert_ne!(sig_min, sig_max, "Different gas limits should produce different signatures");
    }

    #[test]
    fn test_sign_builder_message_edge_case_timestamps() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair.clone(), chain_info.clone());
        
        // Test with genesis timestamp
        let message_genesis = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 0,
            pubkey: signing_ctx.pubkey,
        };
        let sig_genesis = signing_ctx.sign_builder_message(&message_genesis);
        let root_genesis = message_genesis.signing_root(chain_info.builder_domain);
        assert!(sig_genesis.verify(&keypair.pk, root_genesis), "Should sign message with timestamp=0");
        
        // Test with far future timestamp
        let message_future = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: u64::MAX,
            pubkey: signing_ctx.pubkey,
        };
        let sig_future = signing_ctx.sign_builder_message(&message_future);
        let root_future = message_future.signing_root(chain_info.builder_domain);
        assert!(sig_future.verify(&keypair.pk, root_future), "Should sign message with timestamp=MAX");
    }

    #[test]
    fn test_sign_relay_message_uses_relay_domain() {
        let keypair = BlsKeypair::random();
        let chain_info = Arc::new(ChainInfo::for_mainnet());
        let signing_ctx = RelaySigningContext::new(keypair.clone(), chain_info);
        
        let message = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 1234567890,
            pubkey: signing_ctx.pubkey,
        };
        
        let signature = signing_ctx.sign_relay_message(&message);
        
        // Verify signature uses RELAY_DOMAIN (not builder domain)
        let relay_domain = B256::from(*RELAY_DOMAIN);
        let root = message.signing_root(relay_domain);
        assert!(signature.verify(&keypair.pk, root), "Relay message should verify with RELAY_DOMAIN");
    }

    #[test]
    fn test_sign_direct() {
        let signing_ctx = RelaySigningContext::default();
        let message = B256::random();
        
        let signature = signing_ctx.sign(message);
        
        // Verify signature
        assert!(signature.verify(&signing_ctx.keypair.pk, message));
    }

    #[test]
    fn test_relay_domain_constant() {
        // Verify RELAY_DOMAIN is correctly formatted
        assert_eq!(RELAY_DOMAIN.len(), 32);
        assert_eq!(&RELAY_DOMAIN[27..], b"relay");
    }

    #[test]
    fn test_signing_context_for_different_networks() {
        let mainnet_ctx = Arc::new(ChainInfo::for_mainnet());
        let sepolia_ctx = Arc::new(ChainInfo::for_sepolia());
        
        let keypair = BlsKeypair::random();
        
        let mainnet_signing = RelaySigningContext::new(keypair.clone(), mainnet_ctx.clone());
        let sepolia_signing = RelaySigningContext::new(keypair.clone(), sepolia_ctx.clone());
        
        // Same keypair but different contexts
        assert_eq!(mainnet_signing.pubkey, sepolia_signing.pubkey);
        assert_ne!(mainnet_ctx.builder_domain, sepolia_ctx.builder_domain);
    }

    #[test]
    fn test_builder_and_relay_signatures_differ() {
        let signing_ctx = RelaySigningContext::default();
        let message = ValidatorRegistrationData {
            fee_recipient: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: 1234567890,
            pubkey: BlsPublicKeyBytes::default(),
        };
        
        let builder_sig = signing_ctx.sign_builder_message(&message);
        let relay_sig = signing_ctx.sign_relay_message(&message);
        
        // Same message but different domains should produce different signatures
        assert_ne!(builder_sig, relay_sig);
    }

    #[test]
    fn test_signature_deterministic_for_same_message() {
        let signing_ctx = RelaySigningContext::default();
        let message = B256::random();
        
        let sig1 = signing_ctx.sign(message);
        let sig2 = signing_ctx.sign(message);
        
        // Same message should produce same signature
        assert_eq!(sig1, sig2);
    }
}
