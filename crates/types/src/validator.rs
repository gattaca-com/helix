use alloy_primitives::{Address, B256};
use lh_bls::{PublicKey, Signature};
use lh_test_random::TestRandom;
use lh_types::{test_utils::TestRandom, ChainSpec, Epoch, SignedRoot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

/// From Lighthouse, replacing PublicKeyBytes with PublicKey
/// Validator registration, for use in interacting with servers implementing the builder API.
#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedValidatorRegistrationData {
    pub message: ValidatorRegistrationData,
    pub signature: Signature,
}

impl SignedValidatorRegistrationData {
    #[cfg(test)]
    pub fn random_for_test() -> Self {
        Self {
            message: ValidatorRegistrationData::random_for_test(),
            signature: Signature::empty(),
        }
    }
}
#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode, TreeHash)]
pub struct ValidatorRegistrationData {
    pub fee_recipient: Address,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub pubkey: PublicKey,
}

impl ValidatorRegistrationData {
    #[cfg(test)]
    pub fn random_for_test() -> Self {
        use crate::random_bls_pubkey;

        Self {
            fee_recipient: Address::ZERO,
            gas_limit: 0,
            timestamp: 0,
            pubkey: random_bls_pubkey(),
        }
    }
}

impl SignedValidatorRegistrationData {
    pub fn verify_signature(&self, spec: &ChainSpec) -> bool {
        let domain = spec.get_builder_domain();
        let message = self.message.signing_root(domain);
        self.signature.verify(&self.message.pubkey, message)
    }
}

/// From Lighthouse, replacing PublicKeyBytes with PublicKey
/// Information about a `BeaconChain` validator.
///
/// Spec v0.12.1
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash, TestRandom,
)]
pub struct Validator {
    pub pubkey: PublicKey,
    pub withdrawal_credentials: B256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub effective_balance: u64,
    pub slashed: bool,
    pub activation_eligibility_epoch: Epoch,
    pub activation_epoch: Epoch,
    pub exit_epoch: Epoch,
    pub withdrawable_epoch: Epoch,
}

impl Validator {
    #[cfg(test)]
    pub fn random_for_test() -> Self {
        use crate::random_bls_pubkey;

        let epoch: Epoch = 0u64.into();

        Self {
            pubkey: random_bls_pubkey(),
            withdrawal_credentials: B256::ZERO,
            effective_balance: 0,
            slashed: false,
            activation_eligibility_epoch: epoch,
            activation_epoch: epoch,
            exit_epoch: epoch,
            withdrawable_epoch: epoch,
        }
    }
}
