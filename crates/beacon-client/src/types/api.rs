use std::collections::HashMap;

use ethereum_consensus::primitives::Bytes32;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::error::ApiError;

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
    pub meta: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct RandaoResponse {
    pub randao: Bytes32,
}
