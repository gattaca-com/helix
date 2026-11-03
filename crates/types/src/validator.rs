use alloy_primitives::{Address, B256};
use lh_types::{Epoch, SignedRoot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{BlsPublicKeyBytes, BlsSignatureBytes, SigError, TestRandom, TestRandomSeed};

/// From Lighthouse, replacing PublicKeyBytes with PublicKey
/// Validator registration, for use in interacting with servers implementing the builder API.
#[derive(PartialEq, Default, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedValidatorRegistrationData {
    pub message: ValidatorRegistrationData,
    pub signature: BlsSignatureBytes,
}

#[derive(PartialEq, Default, Debug, Serialize, Deserialize, Clone, Encode, Decode, TreeHash)]
pub struct ValidatorRegistrationData {
    pub fee_recipient: Address,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub pubkey: BlsPublicKeyBytes,
}

impl SignedRoot for ValidatorRegistrationData {}

impl SignedValidatorRegistrationData {
    pub fn verify_signature(&self, builder_domain: B256) -> Result<(), SigError> {
        crate::gloas::bls_verify(
            self.message.pubkey.as_slice(),
            self.signature.as_slice(),
            self.message.signing_root(builder_domain),
        )
    }
}

/// From Lighthouse, replacing PublicKeyBytes with PublicKey
/// Information about a `BeaconChain` validator.
///
/// Spec v0.12.1
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Validator {
    pub pubkey: BlsPublicKeyBytes,
    pub withdrawal_credentials: B256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub effective_balance: u64,
    pub slashed: bool,
    pub activation_eligibility_epoch: Epoch,
    pub activation_epoch: Epoch,
    pub exit_epoch: Epoch,
    pub withdrawable_epoch: Epoch,
}

impl TestRandom for Validator {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            pubkey: BlsPublicKeyBytes::random(),
            withdrawal_credentials: B256::test_random(),
            effective_balance: u64::random_for_test(rng),
            slashed: bool::random_for_test(rng),
            activation_eligibility_epoch: Epoch::random_for_test(rng),
            activation_epoch: Epoch::random_for_test(rng),
            exit_epoch: Epoch::random_for_test(rng),
            withdrawable_epoch: Epoch::random_for_test(rng),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{get_fixed_pubkey_bytes, get_fixed_secret};

    #[test]
    fn registration_verifies_only_under_its_key_and_domain() {
        let domain = B256::repeat_byte(7);
        let message =
            ValidatorRegistrationData { pubkey: get_fixed_pubkey_bytes(0), ..Default::default() };
        let signature = get_fixed_secret(0).sign(message.signing_root(domain)).serialize().into();
        let mut registration = SignedValidatorRegistrationData { message, signature };

        assert!(registration.verify_signature(domain).is_ok());
        assert!(registration.verify_signature(B256::repeat_byte(8)).is_err());
        registration.message.pubkey = get_fixed_pubkey_bytes(1);
        assert!(registration.verify_signature(domain).is_err());
    }
}
