#[macro_export]
macro_rules! ssz_bytes_wrapper {
    (
        $(#[$attr:meta])*
        $vis:vis struct $Name:ident;
        max  = $Max:ty;
    ) => {
        $(#[$attr])*
        #[derive(Debug, Default, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize, ssz_derive::Encode, ssz_derive::Decode)]
        #[serde(transparent)]
        #[ssz(struct_behaviour = "transparent")]
        $vis struct $Name(pub ::alloy_primitives::Bytes);

        // Deref/DerefMut to inner type
        impl ::core::ops::Deref for $Name {
            type Target = ::alloy_primitives::Bytes;
            #[inline] fn deref(&self) -> &Self::Target { &self.0 }
        }
        impl ::core::ops::DerefMut for $Name {
            #[inline] fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
        }

        // SSZ TreeHash for VariableList<Elem, Max>
        impl ::tree_hash::TreeHash for $Name
        where
            $Max: ::lh_types::Unsigned,
        {
            #[inline]
            fn tree_hash_type() -> ::tree_hash::TreeHashType {
                ::tree_hash::TreeHashType::List
            }

            #[inline]
            fn tree_hash_packed_encoding(&self) -> ::tree_hash::PackedEncoding {
                unreachable!("List should never be packed.")
            }

            #[inline]
            fn tree_hash_packing_factor() -> usize {
                unreachable!("List should never be packed.")
            }

            #[inline]
            fn tree_hash_root(&self) -> ::tree_hash::Hash256 {
                let root = ::tree_hash::merkle_root(self.0.as_ref(), <$Max as ::lh_types::Unsigned>::to_usize().div_ceil(::tree_hash::HASHSIZE));
                ::tree_hash::mix_in_length(&root, self.0.len())
            }
        }

        // Display implementation
        impl ::core::fmt::Display for $Name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        // Convert to SSZ type
        impl $Name {
            pub fn to_ssz_type(&self) -> Result<::ssz_types::VariableList<u8, $Max>, $crate::SszError> {
                ::ssz_types::VariableList::new(self.0.as_ref().to_vec())
            }
        }
    };
}

use libp2p::identity::{PublicKey, secp256k1};
use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error};

/// Serialize a secp256k1 pubkey to hex.
pub(super) fn serialize_pubkey<S>(value: &PublicKey, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let key = value.clone().try_into_secp256k1().map_err(|e| S::Error::custom(e.to_string()))?;
    serializer.serialize_str(&hex::encode(&key.to_bytes()))
}

/// Deserialize a hex-encoded secp256k1 pubkey.
pub(super) fn deserialize_pubkey<'de, D>(d: D) -> Result<PublicKey, D::Error>
where
    D: Deserializer<'de>,
{
    let hex = String::deserialize(d)?;
    let bytes = hex::decode(hex).map_err(|e| D::Error::custom(e.to_string()))?;
    let secp_pubkey = secp256k1::PublicKey::try_from_bytes(&bytes)
        .map_err(|e| D::Error::custom(e.to_string()))?;
    Ok(PublicKey::from(secp_pubkey))
}
