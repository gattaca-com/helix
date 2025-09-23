use alloy_primitives::B256;
use helix_common::Filtering;
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;

pub const GET_HEADER_REQUEST_CUTOFF_MS: i64 = 3000;

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct GetHeaderParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pubkey: BlsPublicKeyBytes,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PreferencesHeader {
    /// Deprecated: This field is maintained for backward compatibility.
    pub censoring: Option<bool>,

    /// Allows validators to indicate whether global or regional filtering should be applied.
    pub filtering: Option<Filtering>,

    /// An optional list of BuilderIDs. If this is set, the relay will only accept
    /// submissions from builders whose public keys are linked to the IDs in this list.
    /// This allows for limiting submissions to a trusted set of builders.
    pub trusted_builders: Option<Vec<String>>,

    /// Allows validators to express a preference for whether a delay should be applied to get
    /// headers or not.
    pub header_delay: Option<bool>,
}
