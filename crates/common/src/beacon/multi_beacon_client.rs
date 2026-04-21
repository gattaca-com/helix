use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::future::join_all;
use helix_types::{ForkName, VersionedSignedProposal};

use crate::{
    beacon::{beacon_client::BeaconClient, error::BeaconClientError, types::BroadcastValidation},
    chain_info::ChainInfo,
    spawn_tracked,
};

#[derive(Clone)]
pub struct MultiBeaconClient {
    pub beacon_clients: Arc<Vec<Arc<BeaconClient>>>,
    pub best_index: Arc<AtomicUsize>,
}

impl MultiBeaconClient {
    pub fn new(beacon_clients: Vec<Arc<BeaconClient>>) -> Self {
        Self { beacon_clients: Arc::new(beacon_clients), best_index: Arc::new(AtomicUsize::new(0)) }
    }

    /// Returns an iterator over beacon clients starting from the best-synced one.
    pub fn beacon_clients_by_last_response(&self) -> impl Iterator<Item = &Arc<BeaconClient>> {
        let start = self.best_index.load(Ordering::Relaxed);
        self.beacon_clients[start..].iter().chain(self.beacon_clients[..start].iter())
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, BeaconClientError> {
        let mut last_error = None;
        for client in self.beacon_clients_by_last_response() {
            match client.get_chain_info().await {
                Ok(chain_info) => return Ok(chain_info),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    /// Publishes the signed beacon block to all beacon clients; returns on first success.
    pub async fn publish_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<(), BeaconClientError> {
        let handles = self
            .beacon_clients
            .iter()
            .map(|client| {
                let client = client.clone();
                let block = block.clone();
                let broadcast_validation = broadcast_validation.clone();
                spawn_tracked!(async move {
                    client.publish_block(block, broadcast_validation, fork).await
                })
            })
            .collect::<Vec<_>>();

        let mut last_error: Option<BeaconClientError> = None;
        for res in (join_all(handles).await).into_iter().flatten() {
            match res {
                Ok(202) => {
                    last_error = Some(BeaconClientError::BlockIntegrationFailed);
                }
                Ok(_) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }
}
