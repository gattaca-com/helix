use alloy_primitives::B256;
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct InclusionListPathParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pub_key: BlsPublicKeyBytes,
}
