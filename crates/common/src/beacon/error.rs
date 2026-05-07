use std::{error::Error, fmt};

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::http::error::HttpClientError;

#[derive(Debug, thiserror::Error)]
pub enum BeaconClientError {
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

    #[error("block integration failed")]
    BlockIntegrationFailed,

    #[error("HTTP client error: {0}")]
    HttpError(#[from] HttpClientError),

    #[error("HTTP request build error: {0}")]
    HttpBuildError(#[from] http::Error),

    #[error("block validation failed: {0}")]
    BlockValidationFailed(String),
}

impl BeaconClientError {
    /// Returns true when the beacon node explicitly rejected the block due to its content
    /// (e.g. equivocation, invalid state transition). Transient errors (timeouts, network
    /// failures, node unavailable, 5xx) return false.
    pub fn is_block_content_error(&self) -> bool {
        match self {
            BeaconClientError::Api(e) => e.is_client_error(),
            BeaconClientError::BlockValidationFailed(_) => true,
            _ => false,
        }
    }
}

impl IntoResponse for BeaconClientError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let code = StatusCode::BAD_REQUEST;
        (code, Json(ApiError::ErrorMessage { code: code.as_u16(), message })).into_response()
    }
}

// NOTE: `IndexedError` must come before `ErrorMessage` so
// the `serde(untagged)` machinery does not greedily match it first.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum ApiError {
    IndexedError { code: u16, message: String, failures: Vec<IndexedError> },
    ErrorMessage { code: u16, message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IndexedError {
    index: usize,
    message: String,
}

impl ApiError {
    fn code(&self) -> u16 {
        match self {
            Self::IndexedError { code, .. } | Self::ErrorMessage { code, .. } => *code,
        }
    }

    pub fn is_client_error(&self) -> bool {
        let code = self.code();
        code >= 400 && code < 500
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ErrorMessage { message, .. } => write!(f, "{message}"),
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
