use axum::http::HeaderMap;

pub trait MetadataProvider: Send + Sync + Clone + 'static {
    fn get_metadata(&self, headers: &HeaderMap) -> Option<String>;
}

#[derive(Default, Clone)]
pub struct DefaultMetadataProvider;
impl DefaultMetadataProvider {
    pub fn new() -> Self {
        DefaultMetadataProvider
    }
}

impl MetadataProvider for DefaultMetadataProvider {
    fn get_metadata(&self, headers: &HeaderMap) -> Option<String> {
        headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string())
    }
}
