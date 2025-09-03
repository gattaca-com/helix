use axum::{
    extract::{ws::WebSocket, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension,
};

pub(crate) mod messages;

#[derive(Debug, Clone)]
pub struct P2pApi;

impl P2pApi {
    #[tracing::instrument(skip_all)]
    pub async fn p2p_connect(
        Extension(api): Extension<P2pApi>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, P2pApiError> {
        println!("got request");
        Ok(ws.on_upgrade(async move |socket| {
            let api = api.clone();
            api.handle_ws_connection(socket).await;
        }))
    }

    async fn handle_ws_connection(&self, mut socket: WebSocket) {
        while let Some(Ok(msg)) = socket.recv().await {
            println!("{msg:?}");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum P2pApiError {
    #[error("internal server error")]
    Foo,
}

impl IntoResponse for P2pApiError {
    fn into_response(self) -> Response {
        let code = match self {
            P2pApiError::Foo => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (code, self.to_string()).into_response()
    }
}
