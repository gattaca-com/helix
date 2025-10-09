use helix_common::Filtering;
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;

pub const GET_HEADER_REQUEST_CUTOFF_MS: i64 = 3000;

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

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct UpdateValidatorPreferencesParams {
    pub validators: Vec<UpdateValidatorPreferencesPayload>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct UpdateValidatorPreferencesPayload {
    pub pubkey: BlsPublicKeyBytes,

    pub preferences: ValidatorPreferenceUpdate,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct ValidatorPreferenceUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filtering: Option<Filtering>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_builders: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_delay: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub gossip_blobs: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_inclusion_lists: Option<bool>,
}
