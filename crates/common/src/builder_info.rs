use ethereum_consensus::{primitives::U256, serde::as_str};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct BuilderInfo {
    #[serde(with = "as_str")]
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
}
