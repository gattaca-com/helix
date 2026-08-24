//! Builder-API-only types introduced for Gloas (ePBS), used to authenticate
//! `getExecutionPayloadBid` and `submitBuilderPreferences` requests, and to carry a
//! proposer's per-builder payment preferences.
//!
//! See <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/validator.md#new-containers>.

use alloy_primitives::B256;
use lh_types::SignedRoot;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use ssz_types::typenum::U4096;
use tree_hash_derive::TreeHash;

use crate::{BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes, SigError};

crate::ssz_bytes_wrapper! {
    /// ByteList[MAX_DATA_SIZE], MAX_DATA_SIZE = 4096
    pub struct RequestAuthData;
    max = U4096;
}

/// Authenticates a `getExecutionPayloadBid` or `submitBuilderPreferences` request. Signed
/// under `DOMAIN_REQUEST_AUTH`, distinct from the in-protocol `DOMAIN_BEACON_BUILDER`.
#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode, TreeHash)]
pub struct BuilderRequestAuth {
    /// Opaque authentication data agreed with the builder out of band.
    pub data: RequestAuthData,
    /// The proposal slot this request is authorized for.
    #[serde(with = "serde_utils::quoted_u64")]
    pub slot: u64,
}

impl SignedRoot for BuilderRequestAuth {}

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct SignedBuilderRequestAuth {
    pub message: BuilderRequestAuth,
    pub signature: BlsSignatureBytes,
}

impl SignedBuilderRequestAuth {
    /// `pubkey` is resolved from the `proposer_pubkey` path parameter, not carried inside
    /// `BuilderRequestAuth` itself. `domain` is `ChainInfo::request_auth_domain`.
    pub fn verify_signature(
        &self,
        pubkey: &BlsPublicKeyBytes,
        domain: B256,
    ) -> Result<(), SigError> {
        let signature = BlsSignature::deserialize(self.signature.as_slice())
            .map_err(|_| SigError::InvalidBlsSignatureBytes)?;
        let pubkey = BlsPublicKey::deserialize(pubkey.as_slice())
            .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;

        let message = self.message.signing_root(domain);
        if !signature.verify(&pubkey, message) {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }
}

/// A proposer's per-builder payment preferences, submitted via `submitBuilderPreferences`.
#[derive(PartialEq, Debug, Default, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct BuilderPreferences {
    /// Maximum execution-layer payment, in Gwei, the proposer will accept from this builder.
    #[serde(with = "serde_utils::quoted_u64")]
    pub max_execution_payment: u64,
}

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone, Encode, Decode)]
pub struct BuilderPreferencesRequest {
    pub preferences: BuilderPreferences,
    pub auth: SignedBuilderRequestAuth,
}

#[cfg(test)]
mod tests {
    use ssz::{Decode, Encode};

    use super::*;
    use crate::BlsKeypair;

    fn sample_request_auth() -> BuilderRequestAuth {
        BuilderRequestAuth { data: RequestAuthData(vec![1, 2, 3, 4].into()), slot: 123 }
    }

    #[test]
    fn request_auth_json_round_trip() {
        let auth = sample_request_auth();
        let json = serde_json::to_string(&auth).unwrap();
        assert_eq!(auth, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn request_auth_ssz_round_trip() {
        let auth = sample_request_auth();
        let bytes = auth.as_ssz_bytes();
        assert_eq!(auth, BuilderRequestAuth::from_ssz_bytes(&bytes).unwrap());
    }

    #[test]
    fn builder_preferences_request_json_round_trip() {
        let request = BuilderPreferencesRequest {
            preferences: BuilderPreferences { max_execution_payment: 42 },
            auth: SignedBuilderRequestAuth {
                message: sample_request_auth(),
                signature: BlsSignatureBytes::default(),
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(request, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn signed_request_auth_signature_round_trips() {
        let keypair = BlsKeypair::random();
        let domain = B256::repeat_byte(7);
        let message = sample_request_auth();
        let root = message.signing_root(domain);
        let signature = keypair.sk.sign(root);

        let signed = SignedBuilderRequestAuth { message, signature: signature.serialize().into() };
        let pubkey: BlsPublicKeyBytes = keypair.pk.serialize().into();

        signed.verify_signature(&pubkey, domain).expect("valid signature should verify");
        signed
            .verify_signature(&pubkey, B256::repeat_byte(8))
            .expect_err("wrong domain should fail verification");
    }
}
