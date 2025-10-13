use alloy_primitives::{Address, B256};
use lh_types::{Epoch, SignedRoot, test_utils::TestRandom};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes, SigError, TestRandomSeed,
};

/// From Lighthouse, replacing PublicKeyBytes with PublicKey
/// Validator registration, for use in interacting with servers implementing the builder API.
#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedValidatorRegistrationData {
    pub message: ValidatorRegistrationData,
    pub signature: BlsSignatureBytes,
}

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode, TreeHash)]
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
        let signature = BlsSignature::deserialize(self.signature.as_slice())
            .map_err(|_| SigError::InvalidBlsSignatureBytes)?;
        let pubkey = BlsPublicKey::deserialize(self.message.pubkey.as_slice())
            .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;

        let message = self.message.signing_root(builder_domain);
        if !signature.verify(&pubkey, message) {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
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
