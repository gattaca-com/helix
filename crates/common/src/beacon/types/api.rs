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
