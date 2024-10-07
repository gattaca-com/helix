use ethereum_consensus::{builder::SignedValidatorRegistration, types::mainnet::ExecutionPayload, Fork};

use crate::{validator_preferences::ValidatorPreferences, versioned_payload::PayloadAndBlobs};

#[derive(Debug, serde::Serialize)]
#[serde(tag = "version", content = "data")]
pub enum GetPayloadResponse {
    #[serde(rename = "bellatrix")]
    Bellatrix(ExecutionPayload),
    #[serde(rename = "capella")]
    Capella(ExecutionPayload),
    #[serde(rename = "deneb")]
    Deneb(PayloadAndBlobs),
}

impl GetPayloadResponse {
    pub fn try_from_execution_payload(exec_payload: &PayloadAndBlobs) -> Option<Self> {
        match exec_payload.execution_payload.version() {
            Fork::Capella => Some(GetPayloadResponse::Capella(exec_payload.execution_payload.clone())),
            Fork::Bellatrix => Some(GetPayloadResponse::Bellatrix(exec_payload.execution_payload.clone())),
            Fork::Deneb => Some(GetPayloadResponse::Deneb(exec_payload.clone())),
            _ => None,
        }
    }
}

impl<'de> serde::Deserialize<'de> for GetPayloadResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Capella(inner))
        }
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Deneb(inner))
        }
        if let Ok(inner) = <_ as serde::Deserialize>::deserialize(&value) {
            return Ok(Self::Bellatrix(inner))
        }
        Err(serde::de::Error::custom("no variant could be deserialized from input for GetPayloadResponse"))
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ValidatorRegistrationInfo {
    pub registration: SignedValidatorRegistration,
    pub preferences: ValidatorPreferences,
}
