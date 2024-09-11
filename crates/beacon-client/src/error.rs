use std::{error::Error, fmt};

use axum::{
    http::{status::InvalidStatusCode, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use ethereum_consensus::ssz;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum BeaconClientError {
    #[error("Reqwest error: {0}")]
    ReqwestError(#[from] reqwest::Error),

    #[error("URL parse error: {0}")]
    UrlError(#[from] url::ParseError),

    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("error from API: {0}")]
    Api(#[from] ApiError),

    #[error("missing expected data in response: {0}")]
    MissingExpectedData(String),

    #[error("beacon node unavailable")]
    BeaconNodeUnavailable,

    #[error("block validation failed")]
    BlockValidationFailed,

    #[error("block integration failed")]
    BlockIntegrationFailed,

    #[error("beacon node syncing")]
    BeaconNodeSyncing,

    #[error("channel error")]
    ChannelError,

    #[error("block ssz-serialization error: {0}")]
    SszSerializationError(#[from] ssz::prelude::SerializeError),

    #[error("Error publishing block: {0}")]
    BlockPublishError(String),

    #[error("Error initializing broadcaster: {0}")]
    BroadcasterInitError(String),

    #[error("Request not supported")]
    RequestNotSupported,
}

impl IntoResponse for BeaconClientError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let code = StatusCode::BAD_REQUEST;
        (code, Json(ApiError::ErrorMessage { code, message })).into_response()
    }
}

// NOTE: `IndexedError` must come before `ErrorMessage` so
// the `serde(untagged)` machinery does not greedily match it first.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum ApiError {
    IndexedError {
        #[serde(with = "helix_utils::serde::axum_as_u16")]
        code: StatusCode,
        message: String,
        failures: Vec<IndexedError>,
    },
    ErrorMessage {
        #[serde(with = "helix_utils::serde::axum_as_u16")]
        code: StatusCode,
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IndexedError {
    index: usize,
    message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ErrorMessage { message, .. } => {
                write!(f, "{message}")
            }
            Self::IndexedError { message, failures, .. } => {
                write!(f, "{message}: ")?;
                for failure in failures {
                    write!(f, "{failure:?}, ")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ApiError {}

impl<'a> TryFrom<(u16, &'a str)> for ApiError {
    type Error = InvalidStatusCode;

    fn try_from((code, message): (u16, &'a str)) -> Result<Self, Self::Error> {
        let code = StatusCode::from_u16(code)?;
        Ok(Self::ErrorMessage { code, message: message.to_string() })
    }
}
