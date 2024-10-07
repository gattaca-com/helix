use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub enum Encoding {
    Json,
    Ssz,
    JsonGzip,
    SszGzip,
}

impl Encoding {
    pub fn to_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Encoding::Json => request.header("Content-Type", "application/json"),
            Encoding::Ssz => request.header("Content-Type", "application/octet-stream"),
            Encoding::JsonGzip => request.header("Content-Type", "application/json").header("Content-Encoding", "gzip"),
            Encoding::SszGzip => request.header("Content-Type", "application/octet-stream").header("Content-Encoding", "gzip"),
        }
    }
}
