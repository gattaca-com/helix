use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::beacon::error::ApiError;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(bound = "T: Serialize + serde::de::DeserializeOwned")]
#[serde(untagged)]
pub enum ApiResult<T: Serialize + DeserializeOwned> {
    Ok(T),
    Err(ApiError),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(bound = "T: Serialize + serde::de::DeserializeOwned")]
pub struct BeaconResponse<T: Serialize + DeserializeOwned> {
    pub data: T,
    #[serde(flatten)]
    pub meta: FxHashMap<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BuilderSummary {
    #[serde(with = "serde_utils::quoted_u64")]
    pub index: u64,
    pub status: BuilderStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuilderStatus {
    Pending,
    Active,
    Exited,
}
