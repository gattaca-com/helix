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
