//! Builder-API-only types introduced for Gloas (ePBS), used to authenticate
//! `getExecutionPayloadBid` and `submitBuilderPreferences` requests, and to carry a
//! proposer's per-builder payment preferences.
//!
//! See <https://github.com/ethereum/builder-specs/blob/main/specs/gloas/validator.md#new-containers>.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use silver_ssz::merkle::{merkleize, merkleize_bytes, mix_in_length, uint64_chunk};
use ssz_derive::{Decode, Encode};
use ssz_types::typenum::{U4096, Unsigned};
use tree_hash_derive::TreeHash;

use crate::{BlsPublicKeyBytes, BlsSignatureBytes, SigError};

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
        let data = &self.message.data;
        // Decoding does not enforce the SSZ limit; `merkleize_bytes` requires
        // the input to fit within its chunk capacity.
        if data.len() > U4096::USIZE {
            return Err(SigError::InvalidBlsSignature);
        }
        let root = merkleize(&[
            mix_in_length(&merkleize_bytes(data, U4096::USIZE / 32), data.len()),
            uint64_chunk(self.message.slot),
        ]);
        crate::gloas::bls_verify(
            pubkey.as_slice(),
            self.signature.as_slice(),
            crate::gloas::signing_root(B256::from(root), domain),
        )
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
    use rand::RngCore;
    use ssz::{Decode, Encode};
    use tree_hash::TreeHash;

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
        let pubkey: BlsPublicKeyBytes = keypair.pk.serialize().into();
        let domain = B256::repeat_byte(7);
        for len in [0, 1, 32, 33, 4096] {
            let mut data = vec![0; len];
            rand::rng().fill_bytes(&mut data);
            let message = BuilderRequestAuth { data: RequestAuthData(data.into()), slot: 123 };
            let object_root = message.tree_hash_root();
            let root = lh_types::SigningData { object_root, domain }.tree_hash_root();
            let signature = keypair.sk.sign(root).serialize().into();
            let signed = SignedBuilderRequestAuth { message, signature };

            signed.verify_signature(&pubkey, domain).expect("valid signature should verify");
            signed
                .verify_signature(&pubkey, B256::repeat_byte(8))
                .expect_err("wrong domain should fail verification");
        }

        let data = vec![1; U4096::USIZE + 1];
        let object_root = B256::from(merkleize(&[
            mix_in_length(&merkleize_bytes(&data, U4096::USIZE / 32), data.len()),
            uint64_chunk(123),
        ]));
        let root = lh_types::SigningData { object_root, domain }.tree_hash_root();
        let signature = keypair.sk.sign(root).serialize().into();
        let message = BuilderRequestAuth { data: RequestAuthData(data.into()), slot: 123 };
        SignedBuilderRequestAuth { message, signature }
            .verify_signature(&pubkey, domain)
            .expect_err("data over the limit should fail verification");
    }
}
