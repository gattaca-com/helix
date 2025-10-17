use alloy_rlp::Bytes;
use futures::{Sink, Stream};

pub(crate) type WSMessage = tokio_tungstenite::tungstenite::Message;

type TungsteniteSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Wrapper over either an [`axum`] or [`tokio_tungstenite`] WebSocket.
///
/// Implements both [`Stream`] and [`Sink`] traits, by delegating to the underlying socket.
pub(crate) enum PeerSocket {
    Axum(axum::extract::ws::WebSocket),
    Tungstenite(TungsteniteSocket),
}

impl From<axum::extract::ws::WebSocket> for PeerSocket {
    fn from(socket: axum::extract::ws::WebSocket) -> Self {
        PeerSocket::Axum(socket)
    }
}

impl From<TungsteniteSocket> for PeerSocket {
    fn from(socket: TungsteniteSocket) -> Self {
        PeerSocket::Tungstenite(socket)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("axum error: {_0}")]
    Axum(#[from] axum::Error),
    #[error("tungstenite error: {_0}")]
    Tungstenite(#[from] tokio_tungstenite::tungstenite::Error),
}

impl Stream for PeerSocket {
    type Item = Result<WSMessage, Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.get_mut() {
            PeerSocket::Axum(ws) => std::pin::pin!(ws)
                .poll_next(cx)
                .map_ok(convert_from_axum_message)
                .map_err(Error::Axum),
            PeerSocket::Tungstenite(ws) => {
                std::pin::pin!(ws).poll_next(cx).map_err(Error::Tungstenite)
            }
        }
    }
}

impl Sink<WSMessage> for PeerSocket {
    type Error = Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            PeerSocket::Axum(ws) => std::pin::pin!(ws).poll_ready(cx).map_err(Error::Axum),
            PeerSocket::Tungstenite(ws) => {
                std::pin::pin!(ws).poll_ready(cx).map_err(Error::Tungstenite)
            }
        }
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: WSMessage) -> Result<(), Self::Error> {
        match self.get_mut() {
            PeerSocket::Axum(ws) => {
                std::pin::pin!(ws).start_send(convert_to_axum_message(item)).map_err(Error::Axum)
            }
            PeerSocket::Tungstenite(ws) => {
                std::pin::pin!(ws).start_send(item).map_err(Error::Tungstenite)
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            PeerSocket::Axum(ws) => std::pin::pin!(ws).poll_flush(cx).map_err(Error::Axum),
            PeerSocket::Tungstenite(ws) => {
                std::pin::pin!(ws).poll_flush(cx).map_err(Error::Tungstenite)
            }
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            PeerSocket::Axum(ws) => std::pin::pin!(ws).poll_close(cx).map_err(Error::Axum),
            PeerSocket::Tungstenite(ws) => {
                std::pin::pin!(ws).poll_close(cx).map_err(Error::Tungstenite)
            }
        }
    }
}

fn convert_from_axum_message(msg: axum::extract::ws::Message) -> WSMessage {
    match msg {
        axum::extract::ws::Message::Binary(bytes) => WSMessage::Binary(bytes),
        axum::extract::ws::Message::Text(utf8_bytes) => {
            WSMessage::Text(Bytes::from(utf8_bytes).try_into().expect("already checked"))
        }
        axum::extract::ws::Message::Ping(bytes) => WSMessage::Ping(bytes),
        axum::extract::ws::Message::Pong(bytes) => WSMessage::Pong(bytes),
        axum::extract::ws::Message::Close(close_frame) => WSMessage::Close(close_frame.map(|cs| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: cs.code.into(),
                reason: Bytes::from(cs.reason).try_into().expect("already checked"),
            }
        })),
    }
}

fn convert_to_axum_message(msg: WSMessage) -> axum::extract::ws::Message {
    match msg {
        WSMessage::Binary(bytes) => axum::extract::ws::Message::Binary(bytes),
        WSMessage::Text(utf8_bytes) => axum::extract::ws::Message::Text(
            Bytes::from(utf8_bytes).try_into().expect("already checked"),
        ),
        WSMessage::Ping(bytes) => axum::extract::ws::Message::Ping(bytes),
        WSMessage::Pong(bytes) => axum::extract::ws::Message::Pong(bytes),
        WSMessage::Close(close_frame) => {
            axum::extract::ws::Message::Close(close_frame.map(|cs| axum::extract::ws::CloseFrame {
                code: cs.code.into(),
                reason: Bytes::from(cs.reason).try_into().expect("already checked"),
            }))
        }
        WSMessage::Frame(frame) => axum::extract::ws::Message::Binary(frame.into_payload()),
    }
}
