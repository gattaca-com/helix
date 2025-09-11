use std::sync::Arc;

use helix_common::{api::builder_api::InclusionList, signing::RelaySigningContext, P2PConfig};
use helix_types::BlsPublicKey;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tracing::{error, info, warn};

use crate::{
    messages::P2PMessage,
    request_handlers::{InclusionListRequest, P2PApiRequest},
};

pub(crate) mod inclusion_lists;
mod message_handler;
pub mod messages;
mod request_handlers;
mod socket;

pub struct P2PApi {
    broadcast_tx: broadcast::Sender<P2PMessage>,
    api_requests_tx: mpsc::Sender<P2PApiRequest>,
    signing_context: Arc<RelaySigningContext>,
    p2p_config: P2PConfig,
}

impl P2PApi {
    /// Creates a new instance.
    /// Starts new tasks for starting new connections and handling incoming messages.
    pub fn new(p2p_config: P2PConfig, signing_context: Arc<RelaySigningContext>) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(100);
        let (api_requests_tx, api_requests_rx) = mpsc::channel(2000);
        let this = Arc::new(Self { p2p_config, broadcast_tx, api_requests_tx, signing_context });

        // If P2P is disabled, return the service without starting any tasks.
        // This will cause all requests to be ignored.
        if !this.p2p_config.is_enabled {
            info!("P2P module is disabled.");
            return this;
        }
        if this.p2p_config.peers.is_empty() {
            warn!("P2P module is enabled but no peers were configured.");
            return this;
        }
        // Validate configuration and fail if it's invalid
        this.p2p_config.validate();

        for peer_config in &this.p2p_config.peers {
            let peer_pubkey = peer_config.pubkey;
            // Parse URL and try to turn into a request ahead-of-time, panicking on error
            let request = url_to_client_request(&peer_config.url);
            // Verify serialized public key is valid
            let pubkey = BlsPublicKey::deserialize(peer_pubkey.as_ref())
                .inspect_err(
                    |e| error!(err=?e, pubkey=%peer_pubkey, "failed to deserialize peer pubkey"),
                )
                .expect("pubkey should be valid");

            // If the peer's pubkey is less than ours, don't try to connect.
            // Imposing an order on the pubkeys prevents redundant connections between peers.
            if peer_pubkey > this.signing_context.pubkey {
                tokio::spawn(this.clone().connect_to_peer(request, pubkey, peer_pubkey));
            }
        }
        tokio::spawn(this.clone().handle_requests(api_requests_rx));
        info!(peer_count=%this.p2p_config.peers.len(), "Initialized P2P module");
        this
    }

    pub fn is_enabled(&self) -> bool {
        self.p2p_config.is_enabled && !self.p2p_config.peers.is_empty()
    }

    pub async fn share_inclusion_list(
        &self,
        slot: u64,
        inclusion_list: InclusionList,
    ) -> Option<InclusionList> {
        // Skip P2P consensus if P2P is disabled
        if !self.is_enabled() {
            return Some(inclusion_list);
        }
        let (result_tx, result_rx) = oneshot::channel();
        let request = P2PApiRequest::LocalInclusionList(InclusionListRequest {
            slot,
            inclusion_list,
            result_tx,
        });
        // Send request to P2P API
        if let Err(err) = self.api_requests_tx.send(request).await {
            // If API service is unavailable, just return the original IL and log a warning
            warn!("failed to send inclusion list to P2P API");
            match err.0 {
                P2PApiRequest::LocalInclusionList(request) => {
                    return Some(request.inclusion_list);
                }
                _ => unreachable!("the returned value is an inclusion list request"),
            }
        }
        // If API service drops the channel, return None and log a warning
        result_rx.await.inspect_err(|_| warn!("response channel was dropped")).ok().flatten()
    }
}

fn url_to_client_request(url: &str) -> axum::http::Request<()> {
    let request = url
        .into_client_request()
        .inspect_err(|e| error!(err=?e, %url, "invalid peer URL"))
        .expect("peer URL in config should be valid");
    request
}
