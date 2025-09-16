use std::sync::Arc;

use axum::{extract::WebSocketUpgrade, response::IntoResponse, Extension};
use tracing::{debug, warn};

use crate::RelayNetworkManager;

#[derive(Clone)]
pub struct RelayNetworkApi(Arc<RelayNetworkManager>);

impl RelayNetworkApi {
    pub(crate) fn new(manager: Arc<RelayNetworkManager>) -> Self {
        Self(manager)
    }

    #[tracing::instrument(skip_all)]
    pub async fn connect(
        Extension(api): Extension<RelayNetworkApi>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, std::convert::Infallible> {
        debug!("got new peer connection");
        // Upgrade connection to WebSocket, spawning a new task to handle the connection
        let ws = ws
            .on_failed_upgrade(|error| warn!(%error, "websocket upgrade failed"))
            .on_upgrade(|socket| api.0.on_peer_connection(socket.into()));

        Ok(ws)
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_enabled()
    }
}
