use axum::{
    body::{to_bytes, Body},
    extract::{MatchedPath, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use helix_common::{metrics::ApiMetrics, utils::utcnow_ns};

use crate::builder::api::MAX_PAYLOAD_LENGTH;

/// Records status code, timeouts, pending, and latency. This should be the outermost layer
// TODO: unify the two layers and include the streaming body limiter
pub async fn outer_metrics_middleware(mut req: Request, next: Next) -> Response {
    let on_receive_ns = utcnow_ns();
    let Some(endpoint) = req.extensions().get::<MatchedPath>() else {
        return next.run(req).await;
    };

    let mut metric = ApiMetrics::new(endpoint.as_str().to_string());
    req.extensions_mut().insert(on_receive_ns);
    let response = next.run(req).await;
    metric.status(response.status().as_str());

    response
}

/// Records body size. This should be after the RequestBodyLimitLayer
pub async fn inner_metrics_middleware(req: Request, next: Next) -> Response {
    let Some(endpoint) = req.extensions().get::<MatchedPath>() else {
        return next.run(req).await;
    };
    let endpoint = endpoint.as_str().to_string();

    let (req_parts, req_body) = req.into_parts();
    match to_bytes(req_body, MAX_PAYLOAD_LENGTH).await {
        Ok(bytes) => {
            ApiMetrics::size(&endpoint, bytes.len());
            let req = Request::from_parts(req_parts, Body::from(bytes));
            next.run(req).await
        }
        Err(_) => {
            // this should never happen
            StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
    }
}
