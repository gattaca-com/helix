use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use ethereum_consensus::primitives::Root;
use futures::future::join_all;
use helix_common::{
    beacon_api::PublishBlobsRequest, bellatrix::SimpleSerialize,
    signed_proposal::VersionedSignedProposal, ProposerDuty, ValidatorSummary,
};
use tokio::{sync::broadcast::Sender, task::JoinError};
use tracing::{error, warn};

use crate::{
    error::BeaconClientError,
    traits::{BeaconClientTrait, MultiBeaconClientTrait},
    types::{BroadcastValidation, HeadEventData, PayloadAttributesEvent, StateId, SyncStatus},
};

#[derive(Clone)]
pub struct MultiBeaconClient<BeaconClient: BeaconClientTrait + 'static> {
    /// Vec of all beacon clients with a fixed usize ID used when
    /// fetching: `beacon_clients_by_last_response`
    pub beacon_clients: Vec<(usize, Arc<BeaconClient>)>,
    /// The ID of the beacon client with the most recent successful response.
    pub best_beacon_instance: Arc<AtomicUsize>,
}

impl<BeaconClient: BeaconClientTrait> MultiBeaconClient<BeaconClient> {
    pub fn new(beacon_clients: Vec<Arc<BeaconClient>>) -> Self {
        let beacon_clients_with_index = beacon_clients.into_iter().enumerate().collect();

        Self {
            beacon_clients: beacon_clients_with_index,
            best_beacon_instance: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns a list of beacon clients, prioritized by the last successful response.
    ///
    /// The beacon client with the most recent successful response is placed at the
    /// beginning of the returned vector. All other clients maintain their original order.
    pub fn beacon_clients_by_last_response(&self) -> Vec<(usize, Arc<BeaconClient>)> {
        let mut instances = self.beacon_clients.clone();
        let index = self.best_beacon_instance.load(Ordering::Relaxed);
        if index != 0 {
            let pos = instances.iter().position(|(i, _)| *i == index).unwrap();
            instances.swap(0, pos);
        }
        instances
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        consensus_version: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        self.publish_block(block, broadcast_validation, consensus_version).await
    }

    pub fn identifier(&self) -> String {
        "MULTI-BEACON-CLIENT".to_string()
    }
}

#[async_trait]
impl<BeaconClient: BeaconClientTrait> MultiBeaconClientTrait for MultiBeaconClient<BeaconClient> {
    /// Retrieves the sync status from multiple beacon clients and selects the best one.
    ///
    /// The function spawns async tasks to fetch the sync status from each beacon client.
    /// It then selects the sync status with the highest `head_slot`.
    async fn best_sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();

        let handles = clients
            .into_iter()
            .map(|(_, client)| tokio::spawn(async move { client.sync_status().await }))
            .collect::<Vec<_>>();

        let results: Vec<Result<Result<SyncStatus, BeaconClientError>, JoinError>> =
            join_all(handles).await;

        let mut best_sync_status: Option<SyncStatus> = None;
        for join_result in results {
            match join_result {
                Ok(sync_status_result) => match sync_status_result {
                    Ok(sync_status) => {
                        if best_sync_status.as_ref().map_or(true, |current_best| {
                            current_best.head_slot < sync_status.head_slot
                        }) {
                            best_sync_status = Some(sync_status);
                        }
                    }
                    Err(err) => tracing::error!("Failed to get sync status: {err:?}"),
                },
                Err(join_err) => {
                    tracing::error!("Tokio join error for best_sync_status: {join_err:?}")
                }
            }
        }

        best_sync_status.ok_or(BeaconClientError::BeaconNodeUnavailable)
    }

    /// `subscribe_to_head_events` subscribes to head events from all beacon nodes.
    ///
    /// This function swaps async tasks for all beacon clients. Therefore,
    /// a single head event will be received multiple times, likely once for every beacon node.
    async fn subscribe_to_head_events(&self, chan: Sender<HeadEventData>) {
        let clients = self.beacon_clients_by_last_response();

        for (_, client) in clients {
            let chan = chan.clone();
            tokio::spawn(async move {
                if let Err(err) = client.subscribe_to_head_events(chan).await {
                    tracing::error!("Failed to subscribe to head events: {err:?}");
                }
            });
        }
    }

    /// `subscribe_to_payload_attributes_events` subscribes to payload attributes events from all
    /// beacon nodes.
    ///
    /// This function swaps async tasks for all beacon clients. Therefore,
    /// a single payload event will be received multiple times, likely once for every beacon node.
    async fn subscribe_to_payload_attributes_events(&self, chan: Sender<PayloadAttributesEvent>) {
        let clients = self.beacon_clients_by_last_response();

        for (_, client) in clients {
            let chan = chan.clone();
            tokio::spawn(async move {
                if let Err(err) = client.subscribe_to_payload_attributes_events(chan).await {
                    tracing::error!("Failed to subscribe to payload attributes events: {err:?}");
                }
            });
        }
    }

    async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter() {
            match client.get_state_validators(state_id.clone()).await {
                Ok(state_validators) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(state_validators)
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter() {
            match client.get_proposer_duties(epoch).await {
                Ok(proposer_duties) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(proposer_duties)
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    /// Publishes the signed beacon block to multiple beacon clients and returns the result.
    ///
    /// This function publishes a block to all beacon clients.
    /// It will instantly return after the first successful response.
    ///
    /// Follows the spec: [Ethereum 2.0 Beacon APIs documentation](https://ethereum.github.io/beacon-APIs/#/ValidatorRequiredApi/publishBlock).
    async fn publish_block<T: Send + Sync + 'static + SimpleSerialize>(
        &self,
        block: Arc<T>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let num_clients = clients.len();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(num_clients);

        clients.into_iter().for_each(|(i, client)| {
            let block = block.clone();
            let broadcast_validation = broadcast_validation.clone();
            let sender = sender.clone();

            tokio::spawn(async move {
                let res = client.publish_block(block, broadcast_validation, fork).await;
                if let Err(err) = sender.send((i, res)).await {
                    // TODO: we might be able to completely remove this as this should only error if
                    // the receiver is dropped this happens if we've received an
                    // ok response
                    warn!("failed to send publish_block response: {err:?}");
                }
            });
        });

        let mut last_error: Option<BeaconClientError> = None;
        for _ in 0..num_clients {
            if let Some((i, res)) = receiver.recv().await {
                match res {
                    // Should the block fail full validation, a separate success response code (202)
                    // is used to indicate that the block was successfully broadcast but failed
                    // integration.
                    Ok(202) => {
                        last_error = Some(BeaconClientError::BlockIntegrationFailed);
                    }
                    Ok(_) => {
                        self.best_beacon_instance.store(i, Ordering::Relaxed);
                        return Ok(())
                    }
                    Err(err) => {
                        last_error = Some(err);
                    }
                }
            } else {
                error!("Tokio channel closed for publish_block");
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn publish_blobs(
        &self,
        blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        clients.into_iter().for_each(|(_i, client)| {
            let sidecars = blob_sidecars.clone();

            tokio::spawn(async move {
                let res = client.publish_blobs(sidecars).await;
                if let Err(err) = res {
                    match err {
                        BeaconClientError::RequestNotSupported => {}
                        error => warn!(?error, "failed to publish blobs"),
                    }
                }
            });
        });

        Ok(200)
    }
}

#[cfg(test)]
mod multi_beacon_client_tests {
    use super::*;
    use crate::mock_beacon_client::MockBeaconClient;

    #[tokio::test]
    async fn test_beacon_clients_by_last_response() {
        let client1 = Arc::new(MockBeaconClient::new());
        let client2 = Arc::new(MockBeaconClient::new());
        let client3 = Arc::new(MockBeaconClient::new());
        let client4 = Arc::new(MockBeaconClient::new());
        let multi_client = MultiBeaconClient::new(vec![client1, client2, client3, client4]);

        multi_client.best_beacon_instance.store(2, Ordering::Relaxed);
        let clients = multi_client.beacon_clients_by_last_response();

        assert_eq!(clients[0].0, multi_client.beacon_clients[2].0);
        assert_eq!(clients[1].0, multi_client.beacon_clients[1].0);
        assert_eq!(clients[2].0, multi_client.beacon_clients[0].0);
        assert_eq!(clients[3].0, multi_client.beacon_clients[3].0);
    }

    #[tokio::test]
    async fn test_best_sync_status() {
        let client1 = MockBeaconClient::new().with_sync_status(SyncStatus {
            head_slot: 10,
            sync_distance: 0,
            is_syncing: false,
        });
        let client2 = MockBeaconClient::new().with_sync_status(SyncStatus {
            head_slot: 20,
            sync_distance: 0,
            is_syncing: false,
        });

        let multi_client = MultiBeaconClient::new(vec![Arc::new(client1), Arc::new(client2)]);
        let best_status = multi_client.best_sync_status().await.unwrap();

        assert_eq!(best_status.head_slot, 20);
    }

    #[tokio::test]
    async fn test_publish_block_ok() {
        let client1 = Arc::new(MockBeaconClient::new().with_publish_block_response_code(200));
        let client2 = Arc::new(MockBeaconClient::new().with_publish_block_response_code(200));

        let multi_client = MultiBeaconClient::new(vec![client1, client2]);
        let result = multi_client
            .publish_block(
                Arc::new(VersionedSignedProposal::default()),
                Some(BroadcastValidation::default()),
                ethereum_consensus::Fork::Capella,
            )
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_publish_block_fail_validation() {
        let client1 = Arc::new(MockBeaconClient::new().with_publish_block_response_code(202));
        let client2 = Arc::new(MockBeaconClient::new().with_publish_block_response_code(202));

        let multi_client = MultiBeaconClient::new(vec![client1, client2]);
        let result = multi_client
            .publish_block(
                Arc::new(VersionedSignedProposal::default()),
                Some(BroadcastValidation::default()),
                ethereum_consensus::Fork::Capella,
            )
            .await;

        assert!(matches!(result, Err(BeaconClientError::BlockIntegrationFailed)));
    }
}
