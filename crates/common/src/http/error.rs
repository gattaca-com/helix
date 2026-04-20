#[derive(Debug, thiserror::Error)]
pub enum HttpClientError {
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("TLS error: {0}")]
    TlsError(#[from] rustls::Error),

    #[error("URL parse error: {0}")]
    UrlError(#[from] url::ParseError),

    #[error("HTTP error: {0}")]
    HttpError(#[from] http::Error),

    #[error("HTTP request failed: {0}")]
    RequestFailed(String),

    #[error("DNS resolution failed: {0}")]
    DnsError(String),

    #[error("URL has no host")]
    MissingHost,

    #[error("URL has no port")]
    MissingPort,

    #[error("invalid DNS name: {0}")]
    InvalidDnsNameError(#[from] rustls::pki_types::InvalidDnsNameError),

    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
}
