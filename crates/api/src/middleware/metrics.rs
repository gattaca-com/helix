use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use helix_common::metrics::ApiMetrics;

use crate::builder::api::MAX_PAYLOAD_LENGTH;

pub async fn metrics_middleware(req: Request, next: Next) -> Response {
    let endpoint = req.uri().path().to_string();

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
