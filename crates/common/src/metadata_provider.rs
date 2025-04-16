use axum::http::HeaderMap;

pub trait MetadataProvider: Send + Sync {
    fn get_metadata(&self, headers: &HeaderMap) -> Option<String>;
}

pub struct DefaultMetadataProvider;

impl MetadataProvider for DefaultMetadataProvider {
    fn get_metadata(&self, headers: &HeaderMap) -> Option<String> {
        headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
    }
}