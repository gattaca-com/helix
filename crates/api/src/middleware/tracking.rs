use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::Body,
    extract::MatchedPath,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use helix_common::{metrics::ApiMetrics, utils::utcnow_ns, BodyTimingStats, MiddlewareTimings};
use http::header::CONTENT_LENGTH;
use http_body::{Body as HttpBody, Frame};
use http_body_util::Limited;
use pin_project_lite::pin_project;

use crate::builder::api::MAX_PAYLOAD_LENGTH;

pin_project! {
    /// Streaming body wrapper that tracks:
    /// - Bytes read
    /// - Time spent waiting to read the body
    /// - Time spent reading the body
    struct TimedLimited<B> {
        #[pin] inner: B,
        stats: Arc<BodyTimingStats>,
        start_reading_at: Option<Instant>,
        last_pending_at: Option<Instant>,
    }
}

impl<B> TimedLimited<B> {
    fn new(inner: B, stats: Arc<BodyTimingStats>) -> Self {
        Self { inner, stats, start_reading_at: None, last_pending_at: None }
    }
}

impl<B> HttpBody for TimedLimited<B>
where
    B: HttpBody<Data = Bytes>,
    B::Error: Into<axum::BoxError>,
{
    type Data = Bytes;
    type Error = axum::BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();

        if this.start_reading_at.is_none() {
            this.stats.set_start();
            *this.start_reading_at = Some(Instant::now());
        }

        if let Some(t) = this.last_pending_at.take() {
            this.stats.add_wait(t.elapsed());
        }

        let read_start = Instant::now();
        match this.inner.poll_frame(cx) {
            Poll::Pending => {
                *this.last_pending_at = Some(Instant::now());

                Poll::Pending
            }

            Poll::Ready(Some(Ok(frame))) => {
                this.stats.add_read(read_start.elapsed());

                if let Some(data) = frame.data_ref() {
                    this.stats.add_bytes(data.len());
                }

                Poll::Ready(Some(Ok(frame)))
            }

            Poll::Ready(Some(Err(e))) => {
                this.stats.add_read(read_start.elapsed());
                Poll::Ready(Some(Err(e.into())))
            }

            Poll::Ready(None) => {
                this.stats.add_read(read_start.elapsed());
                if let Some(start) = this.start_reading_at.take() {
                    this.stats.set_finish(start.elapsed());
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

pub async fn body_limit_middleware(mut req: Request<Body>, next: Next) -> Response {
    let on_receive_ns = utcnow_ns();
    let Some(endpoint) = req.extensions().get::<MatchedPath>() else {
        return next.run(req).await;
    };

    let mut metric = ApiMetrics::new(endpoint.as_str().to_string());
    let stats = Arc::new(BodyTimingStats::default());

    req.extensions_mut().insert(MiddlewareTimings { on_receive_ns, stats: stats.clone() });
    let res = do_request(req, next, stats.clone()).await;

    metric.record(
        res.status().as_str(),
        stats.size() as usize,
        stats.read_latency(),
        stats.wait_latency(),
        stats.total_latency(),
    );

    res
}

async fn do_request(mut req: Request<Body>, next: Next, stats: Arc<BodyTimingStats>) -> Response {
    if let Some(len) = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        if len as usize > MAX_PAYLOAD_LENGTH {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    }

    if req.body().is_end_stream() {
        return next.run(req).await;
    }

    let orig = std::mem::replace(req.body_mut(), Body::empty());
    let limited = Limited::new(orig, MAX_PAYLOAD_LENGTH);
    *req.body_mut() = Body::new(TimedLimited::new(limited, stats));

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        extract::DefaultBodyLimit,
        http::{Request, StatusCode},
        middleware,
        response::IntoResponse,
        routing::post,
        Router,
    };
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn echo_len(body: Bytes) -> impl IntoResponse {
        (StatusCode::OK, body.len().to_string())
    }

    fn app() -> Router {
        Router::new()
            .route("/upload", post(echo_len))
            .layer(DefaultBodyLimit::disable())
            .layer(middleware::from_fn(body_limit_middleware))
    }

    fn body_from_chunks(chunks: Vec<usize>) -> Body {
        let s = futures::stream::iter(
            chunks.into_iter().map(|n| Ok::<Bytes, std::io::Error>(Bytes::from(vec![0u8; n]))),
        );
        Body::from_stream(s)
    }

    #[tokio::test]
    async fn empty_body_fast_path() {
        let app = app();

        let req = Request::builder().method("POST").uri("/upload").body(Body::empty()).unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"0");
    }

    #[tokio::test]
    async fn content_length_too_large_is_413() {
        let app = app();
        let too_big = (MAX_PAYLOAD_LENGTH as u64) + 1;

        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .header(CONTENT_LENGTH, too_big.to_string())
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn within_limit_stream_ok() {
        let app = app();

        // total exactly MAX_PAYLOAD_LENGTH over multiple frames
        let remain = MAX_PAYLOAD_LENGTH;
        let chunks = vec![remain / 3, remain / 3, remain - 2 * (remain / 3)];
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(body_from_chunks(chunks))
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from(MAX_PAYLOAD_LENGTH.to_string()));
    }

    #[tokio::test]
    async fn exceed_limit_mid_stream_is_413() {
        let app = app();

        // send chunks that cross the limit mid-stream
        let chunks = vec![MAX_PAYLOAD_LENGTH / 2, (MAX_PAYLOAD_LENGTH / 2) + 1];
        let req = Request::builder()
            .method("POST")
            .uri("/upload")
            .body(body_from_chunks(chunks))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
