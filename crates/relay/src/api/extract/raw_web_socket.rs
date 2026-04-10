/// Axum extractor that performs a WebSocket upgrade and yields a sync
/// `tungstenite::WebSocket<std::net::TcpStream>` — intended for handoff to a
/// dedicated OS thread for blocking I/O.
use std::{borrow::Cow, net::TcpStream};

use axum::{
    body::Body,
    extract::FromRequestParts,
    response::{IntoResponse, Response},
};
use http::{
    Method, StatusCode, Version,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    request::Parts,
};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::{
    handshake::derive_accept_key,
    protocol::{self, WebSocketConfig},
};

pub type RawWebSocket = tokio_tungstenite::tungstenite::WebSocket<TcpStream>;

#[must_use]
pub struct RawWebSocketUpgrade {
    config: WebSocketConfig,
    protocol: Option<HeaderValue>,
    sec_websocket_key: Option<HeaderValue>,
    on_upgrade: hyper::upgrade::OnUpgrade,
    sec_websocket_protocol: Option<HeaderValue>,
}

impl RawWebSocketUpgrade {
    pub fn max_message_size(mut self, max: usize) -> Self {
        self.config.max_message_size = Some(max);
        self
    }

    pub fn max_frame_size(mut self, max: usize) -> Self {
        self.config.max_frame_size = Some(max);
        self
    }

    pub fn max_write_buffer_size(mut self, max: usize) -> Self {
        self.config.max_write_buffer_size = max;
        self
    }

    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cow<'static, str>>,
    {
        if let Some(req_protocols) =
            self.sec_websocket_protocol.as_ref().and_then(|p| p.to_str().ok())
        {
            self.protocol = protocols
                .into_iter()
                .map(Into::into)
                .find(|protocol| {
                    req_protocols
                        .split(',')
                        .any(|req_protocol| req_protocol.trim() == protocol.as_ref())
                })
                .map(|protocol| match protocol {
                    Cow::Owned(s) => HeaderValue::from_str(&s).unwrap(),
                    Cow::Borrowed(s) => HeaderValue::from_static(s),
                });
        }
        self
    }

    /// Finalize the upgrade. Spawns a tokio task that awaits the HTTP upgrade,
    /// downcasts to `std::net::TcpStream`, builds a `tungstenite::WebSocket`,
    /// and invokes `callback` synchronously. The callback should move the
    /// `WebSocket` to an OS thread — it must not block the tokio runtime.
    #[must_use = "response must be returned to complete the upgrade"]
    pub fn on_upgrade<C>(self, callback: C) -> Response
    where
        C: FnOnce(RawWebSocket) + Send + 'static,
    {
        let on_upgrade = self.on_upgrade;
        let config = self.config;

        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(_) => return,
            };

            // axum::serve uses auto::Builder which wraps the IO in Rewind<TokioIo<TcpStream>>.
            // Use the auto upgrade downcast helper which unwraps the Rewind layer.
            let parts = hyper_util::server::conn::auto::upgrade::downcast::<
                TokioIo<tokio::net::TcpStream>,
            >(upgraded)
            .expect("upgraded connection must be TokioIo<TcpStream>");
            debug_assert!(
                parts.read_buf.is_empty(),
                "unexpected pre-buffered bytes after WebSocket upgrade"
            );

            let std_tcp =
                parts.io.into_inner().into_std().expect("failed to convert tokio TcpStream to std");

            let ws =
                protocol::WebSocket::from_raw_socket(std_tcp, protocol::Role::Server, Some(config));
            callback(ws);
        });

        #[allow(clippy::declare_interior_mutable_const)]
        const UPGRADE: HeaderValue = HeaderValue::from_static("upgrade");
        #[allow(clippy::declare_interior_mutable_const)]
        const WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");

        let mut response = if let Some(sec_websocket_key) = &self.sec_websocket_key {
            let accept = derive_accept_key(sec_websocket_key.as_bytes());
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(header::CONNECTION, UPGRADE)
                .header(header::UPGRADE, WEBSOCKET)
                .header(header::SEC_WEBSOCKET_ACCEPT, accept)
                .body(Body::empty())
                .unwrap()
        } else {
            Response::new(Body::empty())
        };

        if let Some(protocol) = self.protocol {
            response.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }

        response
    }
}

impl<S> FromRequestParts<S> for RawWebSocketUpgrade
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let sec_websocket_key = if parts.version <= Version::HTTP_11 {
            if parts.method != Method::GET {
                return Err(
                    (StatusCode::METHOD_NOT_ALLOWED, "Request method must be GET").into_response()
                );
            }
            if !header_contains(&parts.headers, header::CONNECTION, "upgrade") {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Connection header did not include 'upgrade'",
                )
                    .into_response());
            }
            if !header_eq(&parts.headers, header::UPGRADE, "websocket") {
                return Err((StatusCode::BAD_REQUEST, "Upgrade header did not include 'websocket'")
                    .into_response());
            }
            Some(
                parts
                    .headers
                    .get(header::SEC_WEBSOCKET_KEY)
                    .ok_or_else(|| {
                        (StatusCode::BAD_REQUEST, "Sec-WebSocket-Key header missing")
                            .into_response()
                    })?
                    .clone(),
            )
        } else {
            if parts.method != Method::CONNECT {
                return Err((StatusCode::METHOD_NOT_ALLOWED, "Request method must be CONNECT")
                    .into_response());
            }
            None
        };

        if !header_eq(&parts.headers, header::SEC_WEBSOCKET_VERSION, "13") {
            return Err((
                StatusCode::BAD_REQUEST,
                "Sec-WebSocket-Version header did not include '13'",
            )
                .into_response());
        }

        let on_upgrade =
            parts.extensions.remove::<hyper::upgrade::OnUpgrade>().ok_or_else(|| {
                (StatusCode::UPGRADE_REQUIRED, "WebSocket request couldn't be upgraded")
                    .into_response()
            })?;

        let sec_websocket_protocol = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();

        Ok(Self {
            config: Default::default(),
            protocol: None,
            sec_websocket_key,
            on_upgrade,
            sec_websocket_protocol,
        })
    }
}

fn header_eq(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool {
    headers.get(&key).is_some_and(|h| h.as_bytes().eq_ignore_ascii_case(value.as_bytes()))
}

fn header_contains(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool {
    headers.get(&key).is_some_and(|h| {
        std::str::from_utf8(h.as_bytes())
            .ok()
            .is_some_and(|s| s.to_ascii_lowercase().contains(value))
    })
}
