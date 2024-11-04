#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ValidatorPreferences {
    /// Apply filtering to the beacon block submissions.
    #[serde(default = "default_filtering")]
    pub filtering: Filtering,
    /// An optional list of BuilderIDs. If this is set, the relay will only accept
    /// submissions from builders whose public keys are linked to the IDs in this list.
    /// This allows for limiting submissions to a trusted set of builders.
    #[serde(default)]
    pub trusted_builders: Option<Vec<String>>,

    /// Allows validators to express a preference for whether a delay should be applied to get
    /// headers or not.
    #[serde(default = "default_header_delay")]
    pub header_delay: bool,

    #[serde(default)]
    pub gossip_blobs: bool,
}

fn default_filtering() -> Filtering {
    Filtering::Global
}
fn default_header_delay() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum Filtering {
    #[default]
    #[serde(rename = "global")]
    Global = 0,
    #[serde(rename = "regional")]
    Regional = 1,
}

impl Filtering {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Filtering::Global),
            1 => Some(Filtering::Regional),
            _ => None,
        }
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Filtering::Global)
    }

    pub fn is_regional(&self) -> bool {
        matches!(self, Filtering::Regional)
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuilderValidatorPreferences {
    pub censoring: bool,
    pub filtering: Filtering,
    pub trusted_builders: Option<Vec<String>>,
}

impl From<ValidatorPreferences> for BuilderValidatorPreferences {
    fn from(preferences: ValidatorPreferences) -> Self {
        Self {
            censoring: preferences.filtering.is_regional(),
            filtering: preferences.filtering,
            trusted_builders: preferences.trusted_builders.clone(),
        }
    }
}

#[test]
fn test_validator_preferences_serde() {
    let preferences = ValidatorPreferences {
        filtering: Filtering::Regional,
        trusted_builders: Some(vec!["builder1".to_string(), "builder2".to_string()]),
        header_delay: false,
        gossip_blobs: true,
    };

    let json = serde_json::to_string(&preferences).unwrap();
    let _deserialized: ValidatorPreferences = serde_json::from_str(&json).unwrap();

    println!("{}", json);
}
