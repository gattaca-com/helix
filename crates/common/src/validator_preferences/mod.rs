#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ValidatorPreferences {
    /// A boolean flag indicating whether the validator requests the relay
    /// to enforce censoring of sanctioned transactions.
    pub censoring: bool,
    /// An optional list of BuilderIDs. If this is set, the relay will only accept
    /// submissions from builders whose public keys are linked to the IDs in this list.
    /// This allows for limiting submissions to a trusted set of builders.
    pub trusted_builders: Option<Vec<String>>,

    /// Allows validators to express a preference for whether a delay should be applied to get headers or not.
    #[serde(default = "default_header_delay")]
    pub header_delay: bool,
}

fn default_header_delay() -> bool {
    true
}
