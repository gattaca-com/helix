use flux::type_hash_derive::type_hash_lock;
use flux_utils::ArrayStr;
use flux_versioned_types::versioned_struct;

// Shared gather envelope: receivers decode helix blobs with their own copy
// of this type. The locked type hashes are the compatibility contract: keep
// them identical, and mirror any new version the receivers add.
versioned_struct!(GatherMeta =>
    #[type_hash_lock(hash = 4630356712850056543)]
    GatherMetaV1 {
        pub slot: u64,
        pub n_blobs: u64,
        pub instance: ArrayStr<32>,
        pub git_hash: ArrayStr<8>,
        pub app: ArrayStr<32>,
    }
);

impl GatherMeta {
    pub fn new(slot: u64, n_blobs: u64, instance: &str, app: &str) -> Self {
        Self {
            slot,
            n_blobs,
            instance: ArrayStr::from_str_truncate(instance),
            git_hash: ArrayStr::from_str_truncate(env!("GIT_HASH")),
            app: ArrayStr::from_str_truncate(app),
        }
    }
}
