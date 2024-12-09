use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use helix_common::{
    api::{PATH_BUILDER_API, PATH_CONSTRAINTS_API, PATH_DATA_API, PATH_PROPOSER_API},
    metrics::ApiMetrics,
};

use crate::builder::api::MAX_PAYLOAD_LENGTH;

use super::rate_limiting::rate_limit_by_ip::replace_dynamic_routes;

pub async fn metrics_middleware(req: Request, next: Next) -> Response {
    let endpoint = req.uri().path();

    if !SUPPORTED_PATHS.iter().any(|path| endpoint.starts_with(path)) {
        return next.run(req).await
    }

    let endpoint = replace_dynamic_routes(endpoint).to_string();

    ApiMetrics::count(&endpoint);
    let _timer = ApiMetrics::timer(&endpoint);

    let (req_parts, req_body) = req.into_parts();

    // we can probably remove the RequestBodyLimitLayer with this
    let response = match to_bytes(req_body, MAX_PAYLOAD_LENGTH).await {
        Ok(bytes) => {
            ApiMetrics::size(&endpoint, bytes.len());

            let req = Request::from_parts(req_parts, Body::from(bytes));
            next.run(req).await
        }
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    ApiMetrics::status(&endpoint, response.status().as_str());

    response
}

const SUPPORTED_PATHS: [&str; 4] =
    [PATH_BUILDER_API, PATH_PROPOSER_API, PATH_DATA_API, PATH_CONSTRAINTS_API];
