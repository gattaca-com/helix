#![allow(clippy::manual_div_ceil)]

use lh_types::Unsigned;
use tree_hash::{Hash256, MerkleHasher, TreeHash, TreeHashType};

// From ssz_types
/// A helper function providing common functionality between the `TreeHash` implementations for
/// `FixedVector` and `VariableList`.
pub fn vec_tree_hash_root<T, N>(vec: &[T]) -> Hash256
where
    T: TreeHash,
    N: Unsigned,
{
    match T::tree_hash_type() {
        TreeHashType::Basic => {
            let mut hasher = MerkleHasher::with_leaves(
                (N::to_usize() + T::tree_hash_packing_factor() - 1) / T::tree_hash_packing_factor(),
            );

            for item in vec {
                hasher
                    .write(&item.tree_hash_packed_encoding())
                    .expect("ssz_types variable vec should not contain more elements than max");
            }

            hasher.finish().expect("ssz_types variable vec should not have a remaining buffer")
        }
        TreeHashType::Container | TreeHashType::List | TreeHashType::Vector => {
            let mut hasher = MerkleHasher::with_leaves(N::to_usize());

            for item in vec {
                hasher
                    .write(item.tree_hash_root().as_slice())
                    .expect("ssz_types vec should not contain more elements than max");
            }

            hasher.finish().expect("ssz_types vec should not have a remaining buffer")
        }
    }
}

#[macro_export]
macro_rules! ssz_list_wrapper {
    (
        $(#[$attr:meta])*
        $vis:vis struct $Name:ident($Inner:ty);
        elem = $Elem:ty;
        max  = $Max:ty;
    ) => {
        $(#[$attr])*
        #[derive(Debug, Default, PartialEq, Clone, Serialize, Deserialize, Encode, Decode)]
        #[serde(transparent)]
        #[ssz(struct_behaviour = "transparent")]
        $vis struct $Name(pub $Inner);

        // Deref/DerefMut to inner type
        impl ::core::ops::Deref for $Name {
            type Target = $Inner;
            #[inline] fn deref(&self) -> &Self::Target { &self.0 }
        }
        impl ::core::ops::DerefMut for $Name {
            #[inline] fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
        }

        // SSZ TreeHash for VariableList<Elem, Max>
        impl ::tree_hash::TreeHash for $Name
        where
            $Elem: ::tree_hash::TreeHash,
            $Inner: ::core::convert::AsRef<[$Elem]>,
            $Max: ::lh_types::Unsigned,
        {
            #[inline]
            fn tree_hash_root(&self) -> ::tree_hash::Hash256 {
                let slice: &[$Elem] = ::core::convert::AsRef::as_ref(&self.0);
                let root = $crate::utils::vec_tree_hash_root::<$Elem, $Max>(slice);
                ::tree_hash::mix_in_length(&root, slice.len())
            }

            #[inline]
            fn tree_hash_type() -> ::tree_hash::TreeHashType {
                ::ssz_types::VariableList::<$Elem, $Max>::tree_hash_type()
            }

            #[inline]
            fn tree_hash_packed_encoding(&self) -> ::tree_hash::PackedEncoding {
                unreachable!("List should never be packed.")
            }

            #[inline]
            fn tree_hash_packing_factor() -> usize {
                ::ssz_types::VariableList::<$Elem, $Max>::tree_hash_packing_factor()
            }
        }
    };
}
