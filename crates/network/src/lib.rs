use std::sync::Arc;

use helix_common::{
    api::builder_api::InclusionList, signing::RelaySigningContext, RelayNetworkConfig,
};
use helix_types::BlsPublicKey;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tracing::{error, info, warn};

use crate::{
    api::RelayNetworkApi,
    event_handlers::{InclusionListEvent, NetworkEvent},
    messages::NetworkMessage,
};

pub mod api;
mod event_handlers;
pub(crate) mod inclusion_lists;
mod message_handler;
pub mod messages;
mod socket;

pub struct RelayNetworkManager {
    broadcast_tx: broadcast::Sender<NetworkMessage>,
    api_events_tx: mpsc::Sender<NetworkEvent>,
    signing_context: Arc<RelaySigningContext>,
    network_config: RelayNetworkConfig,
}

impl RelayNetworkManager {
    /// Creates a new instance.
    /// Starts new tasks for starting new connections and handling incoming messages.
    pub fn new(
        network_config: RelayNetworkConfig,
        signing_context: Arc<RelaySigningContext>,
    ) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(100);
        let (api_events_tx, api_events_rx) = mpsc::channel(100);
        let this = Arc::new(Self { network_config, broadcast_tx, api_events_tx, signing_context });

        // If it's disabled, return the service without starting any tasks.
        // This will cause all events to be ignored.
        if !this.network_config.is_enabled {
            info!("Network module is disabled.");
            return this;
        }
        if this.network_config.peers.is_empty() {
            warn!("Network module is enabled but no peers were configured.");
            return this;
        }
        // Validate configuration and fail if it's invalid
        this.network_config.validate();

        for peer_config in &this.network_config.peers {
            let peer_pubkey = peer_config.pubkey;
            // Parse URL and try to turn into a event ahead-of-time, panicking on error
            let request = url_to_client_event(&peer_config.url);
            // Verify serialized public key is valid
            let _deserialized_pubkey = BlsPublicKey::deserialize(peer_pubkey.as_ref())
                .inspect_err(
                    |e| error!(err=?e, pubkey=%peer_pubkey, "failed to deserialize peer pubkey"),
                )
                .expect("pubkey should be valid");

            // If the peer's pubkey is less than ours, don't try to connect.
            // Imposing an order on the pubkeys prevents redundant connections between peers.
            if peer_pubkey > this.signing_context.pubkey {
                tokio::spawn(this.clone().connect_to_peer(request, peer_pubkey));
            }
        }
        tokio::spawn(this.clone().run_event_handling_loop(api_events_rx));
        info!(peer_count=%this.network_config.peers.len(), "Initialized network module");
        this
    }

    pub fn api(self: &Arc<Self>) -> RelayNetworkApi {
        RelayNetworkApi::new(self.clone())
    }

    pub fn is_enabled(&self) -> bool {
        self.network_config.is_enabled && !self.network_config.peers.is_empty()
    }

    pub async fn share_inclusion_list(
        &self,
        slot: u64,
        inclusion_list: InclusionList,
    ) -> Option<InclusionList> {
        // Skip consensus if disabled
        if !self.is_enabled() {
            return Some(inclusion_list);
        }
        let (result_tx, result_rx) = oneshot::channel();
        let event = NetworkEvent::LocalInclusionList(InclusionListEvent {
            slot,
            inclusion_list,
            result_tx,
        });
        // Send event to API
        if let Err(err) = self.api_events_tx.send(event).await {
            // If API service is unavailable, just return the original IL and log a warning
            warn!("failed to send inclusion list to network API");
            match err.0 {
                NetworkEvent::LocalInclusionList(event) => {
                    return Some(event.inclusion_list);
                }
                _ => unreachable!("the returned value is an inclusion list event"),
            }
        }
        // If API service drops the channel, return None and log a warning
        result_rx.await.inspect_err(|_| warn!("response channel was dropped")).ok().flatten()
    }
}

fn url_to_client_event(url: &str) -> axum::http::Request<()> {
    let request = url
        .into_client_request()
        .inspect_err(|e| error!(err=?e, %url, "invalid peer URL"))
        .expect("peer URL in config should be valid");
    request
}
