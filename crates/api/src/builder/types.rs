use alloy_primitives::B256;
use helix_types::BlsPublicKey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i16)]
pub enum OptimisticVersion {
    #[default]
    NotOptimistic = 0,
    V1 = 1,
    V2 = 2,
    V3 = 3,
}

#[derive(Debug, Deserialize)]
pub struct InclusionListPathParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pub_key: BlsPublicKey,
}
