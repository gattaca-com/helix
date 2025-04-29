use alloy_primitives::U256;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct BuilderInfo {
    #[serde(with = "serde_utils::quoted_u256")]
    pub collateral: U256,
    pub is_optimistic: bool,
    /// Whether the builder is optimistic for regional filtering.
    #[serde(default)]
    pub is_optimistic_for_regional_filtering: bool,
    pub builder_id: Option<String>,
    pub builder_ids: Option<Vec<String>>,
}

impl BuilderInfo {
    pub fn can_process_regional_slot_optimistically(&self) -> bool {
        self.is_optimistic && self.is_optimistic_for_regional_filtering
    }

    pub fn builder_id(&self) -> &str {
        self.builder_id.as_ref().map(|s| s.as_str()).unwrap_or("")
    }
}
