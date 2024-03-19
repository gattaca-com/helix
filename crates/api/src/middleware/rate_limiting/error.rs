//! Error types

use std::{error, fmt};

use axum::{http::StatusCode, response::IntoResponse};

/// The timeout elapsed.
#[derive(Debug, Default)]
pub struct RateLimitExceeded(pub(super) ());

impl RateLimitExceeded {
    /// Construct a new elapsed error
    pub fn new() -> Self {
        RateLimitExceeded(())
    }
}

impl fmt::Display for RateLimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("request rate limit exceeded")
    }
}

impl error::Error for RateLimitExceeded {}

impl IntoResponse for RateLimitExceeded {
    fn into_response(self) -> axum::response::Response {
        StatusCode::TOO_MANY_REQUESTS.into_response()
    }
}
