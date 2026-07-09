use alloy_primitives::U256;
use helix_common::BuilderConfig;
use helix_types::BlsPublicKeyBytes;
use serde::Serialize;

/// Builder entry as exposed to the admin UI. Deliberately excludes the
/// builder's `api_key`.
#[derive(Serialize)]
pub struct BuilderResponse {
    pub pub_key: BlsPublicKeyBytes,
    #[serde(with = "serde_utils::quoted_u256")]
    pub collateral: U256,
    pub is_optimistic: bool,
    pub is_optimistic_for_regional_filtering: bool,
    pub builder_id: Option<String>,
    pub builder_ids: Option<Vec<String>>,
}

impl From<BuilderConfig> for BuilderResponse {
    fn from(config: BuilderConfig) -> Self {
        Self {
            pub_key: config.pub_key,
            collateral: config.builder_info.collateral,
            is_optimistic: config.builder_info.is_optimistic,
            is_optimistic_for_regional_filtering: config
                .builder_info
                .is_optimistic_for_regional_filtering,
            builder_id: config.builder_info.builder_id,
            builder_ids: config.builder_info.builder_ids,
        }
    }
}

#[derive(Serialize)]
pub struct DemotionResponse {
    pub public_key: BlsPublicKeyBytes,
    pub demotion_time_ms: i64,
    pub reason: Option<String>,
    pub block_hash: Option<String>,
    pub slot_number: Option<i32>,
}

#[derive(Serialize)]
pub struct OverviewResponse {
    pub num_network_validators: i64,
    pub num_registered_validators: i64,
    pub num_delivered_payloads: i64,
    pub adjustments_enabled: bool,
    /// `None` when the relay admin API is unreachable.
    pub kill_switch_enabled: Option<bool>,
}
