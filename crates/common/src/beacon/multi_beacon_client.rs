use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use alloy_primitives::B256;
use futures::{Stream, future::join_all, stream::select_all};
use helix_types::{ForkName, VersionedSignedProposal};
use tracing::error;

use crate::{
    ProposerDuty, ValidatorSummary,
    beacon::{
        beacon_client::BeaconClient,
        error::BeaconClientError,
        types::{BroadcastValidation, HeadEventData, PayloadAttributesEvent, StateId, SyncStatus},
    },
    chain_info::ChainInfo,
    metrics::BeaconMetrics,
    spawn_tracked,
};

#[derive(Clone)]
pub struct MultiBeaconClient {
    // never changed after init
    pub beacon_clients: Arc<Vec<Arc<BeaconClient>>>,
    pub best_index: Arc<AtomicUsize>,
}

impl MultiBeaconClient {
    pub fn new(beacon_clients: Vec<Arc<BeaconClient>>) -> Self {
        Self { beacon_clients: Arc::new(beacon_clients), best_index: Arc::new(AtomicUsize::new(0)) }
    }

    /// Returns a list of beacon clients, prioritized by the last successful response.
    ///
    /// The beacon client with the most recent successful response is placed at the
    /// beginning of the returned vector. All other clients maintain their original order.
    pub fn beacon_clients_by_last_response(
        &self,
    ) -> impl Iterator<Item = Arc<BeaconClient>> + use<'_> {
        let start = self.best_index.load(Ordering::Relaxed);
        self.beacon_clients[start..].iter().chain(&self.beacon_clients[..start]).cloned()
    }
}

impl MultiBeaconClient {
    pub fn head_event_stream(&self) -> impl Stream<Item = HeadEventData> + Send + 'static {
        let streams: Vec<Pin<Box<dyn Stream<Item = HeadEventData> + Send>>> = self
            .beacon_clients
            .iter()
            .map(|c| Box::pin(c.sse_stream::<HeadEventData>("head")) as _)
            .collect();
        select_all(streams)
    }

    pub fn payload_attr_stream(
        &self,
    ) -> impl Stream<Item = PayloadAttributesEvent> + Send + 'static {
        let streams: Vec<Pin<Box<dyn Stream<Item = PayloadAttributesEvent> + Send>>> = self
            .beacon_clients
            .iter()
            .map(|c| Box::pin(c.sse_stream::<PayloadAttributesEvent>("payload_attributes")) as _)
            .collect();
        select_all(streams)
    }

    /// Retrieves the sync status from multiple beacon clients and selects the best one.
    pub async fn best_sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        let handles = self
            .beacon_clients
            .iter()
            .map(|client| {
                let client = client.clone();
                spawn_tracked!(async move {
                    let sync_status = client.sync_status().await;
                    let is_synced = sync_status.as_ref().is_ok_and(|s| !s.is_syncing);
                    BeaconMetrics::beacon_sync(client.endpoint(), is_synced);
                    sync_status
                })
            })
            .collect::<Vec<_>>();

        let mut best_sync_status: Option<(usize, SyncStatus)> = None;

        for (i, join_result) in join_all(handles).await.into_iter().enumerate() {
            if let Ok(sync_status_result) = join_result {
                match sync_status_result {
                    Ok(sync_status) => {
                        if best_sync_status.as_ref().is_none_or(|(_, current_best)| {
                            current_best.head_slot < sync_status.head_slot
                        }) {
                            best_sync_status = Some((i, sync_status));
                        }
                    }
                    Err(err) => error!(%err, "failed to get sync status"),
                }
            }
        }

        if let Some((i, sync_status)) = best_sync_status {
            self.best_index.store(i, Ordering::Relaxed);
            Ok(sync_status)
        } else {
            Err(BeaconClientError::BeaconNodeUnavailable)
        }
    }

    pub async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        let mut last_error = None;
        for client in self.beacon_clients_by_last_response() {
            match client.get_state_validators(state_id.clone()).await {
                Ok(state_validators) => return Ok(state_validators),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    pub async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(B256, Vec<ProposerDuty>), BeaconClientError> {
        let mut last_error = None;
        for client in self.beacon_clients_by_last_response() {
            match client.get_proposer_duties(epoch).await {
                Ok(proposer_duties) => return Ok(proposer_duties),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
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
            .beacon_clients_by_last_response()
            .map(|client| {
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

#[cfg(test)]
mod multi_beacon_client_tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::beacon::beacon_client::mock_beacon_node::MockBeaconNode;

    fn mock_beacon_client(head_slot: u64) -> Arc<BeaconClient> {
        let sync_status =
            SyncStatus { head_slot: head_slot.into(), sync_distance: 0, is_syncing: false };

        let mock_node = MockBeaconNode::new();
        mock_node.with_sync_status(&sync_status);

        Arc::new(mock_node.beacon_client())
    }

    #[test]
    fn test_beacon_clients_by_last_response() {
        let multi_client = MultiBeaconClient::new(vec![
            mock_beacon_client(1),
            mock_beacon_client(2),
            mock_beacon_client(100),
            mock_beacon_client(4),
        ]);

        multi_client.best_index.store(2, Ordering::Relaxed);

        let clients = multi_client.beacon_clients_by_last_response().collect::<Vec<_>>();

        assert_eq!(clients[0].endpoint(), multi_client.beacon_clients[2].endpoint());
        assert_eq!(clients[1].endpoint(), multi_client.beacon_clients[3].endpoint());
        assert_eq!(clients[2].endpoint(), multi_client.beacon_clients[0].endpoint());
        assert_eq!(clients[3].endpoint(), multi_client.beacon_clients[1].endpoint());
    }

    #[tokio::test]
    async fn test_best_sync_status() {
        let mock_node_1 = MockBeaconNode::new();
        let client1 = mock_node_1.beacon_client();
        mock_node_1.with_sync_status(&SyncStatus {
            head_slot: 10u64.into(),
            sync_distance: 0,
            is_syncing: false,
        });

        let mock_node_2 = MockBeaconNode::new();
        let client2 = mock_node_2.beacon_client();
        mock_node_2.with_sync_status(&SyncStatus {
            head_slot: 20u64.into(),
            sync_distance: 0,
            is_syncing: false,
        });

        let multi_client = MultiBeaconClient::new(vec![Arc::new(client1), Arc::new(client2)]);
        let best_status = multi_client.best_sync_status().await.unwrap();

        assert_eq!(best_status.head_slot, 20);
        assert_eq!(multi_client.best_index.load(Ordering::Relaxed), 1);
    }
}
