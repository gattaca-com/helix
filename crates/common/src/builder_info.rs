use alloy_primitives::U256;

#[derive(Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct BuilderInfo {
    #[serde(with = "serde_utils::quoted_u256")]
    pub collateral: U256,
    pub is_optimistic: bool,
    /// Whether the builder is optimistic for regional filtering.
    #[serde(default)]
    pub is_optimistic_for_regional_filtering: bool,
    pub builder_id: Option<String>,
    pub builder_ids: Option<Vec<String>>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub skip_sigverify: bool,
    #[serde(default)]
    pub tcp_submissions: bool,
    #[serde(default)]
    pub top_bid_feed: bool,
}

impl BuilderInfo {
    pub fn can_process_regional_slot_optimistically(&self) -> bool {
        self.is_optimistic && self.is_optimistic_for_regional_filtering
    }

    pub fn builder_id(&self) -> &str {
        self.builder_id.as_deref().unwrap_or("")
    }
}

impl core::fmt::Debug for BuilderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BuilderInfo {{ collateral: {:?}, is_optimistic: {:?}, is_optimistic_for_regional_filtering: {:?}, builder_id: {:?}, builder_ids: {:?}, api_key: ***, skip_sigverify: {:?}, tcp_submissions: {:?}, top_bid_feed: {:?} }}",
            self.collateral,
            self.is_optimistic,
            self.is_optimistic_for_regional_filtering,
            self.builder_id,
            self.builder_ids,
            self.skip_sigverify,
            self.tcp_submissions,
            self.top_bid_feed,
        )
    }
}
