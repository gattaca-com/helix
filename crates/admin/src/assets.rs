use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

/// Built frontend assets. `frontend/dist` only contains `.gitkeep` in a fresh
/// checkout; build it with `just admin-frontend-build`. In debug builds
/// rust-embed reads from disk at runtime, in release builds the files are
/// embedded at compile time.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/frontend/dist"]
struct Assets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Unknown paths fall back to index.html so client-side routes deep-link.
    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cache_control = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache_control)],
                file.data,
            )
                .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "frontend not built - run `just admin-frontend-build`",
        )
            .into_response(),
    }
}
