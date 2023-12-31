use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use ethereum_consensus::{phase0::Fork, primitives::Root, types::mainnet::SignedBeaconBlock};
use futures::future::join_all;
use helix_common::{ProposerDuty, ValidatorSummary};
use tokio::{sync::mpsc::Sender, task::JoinError};

use crate::{
    error::BeaconClientError,
    traits::{BeaconClientTrait, MultiBeaconClientTrait},
    types::{
        BlockId, BroadcastValidation, GenesisDetails, HeadEventData, PayloadAttributesEvent,
        RandaoResponse, StateId, SyncStatus,
    },
};

#[derive(Clone)]
pub struct MultiBeaconClient<BeaconClient: BeaconClientTrait + 'static> {
    pub beacon_clients: Vec<Arc<BeaconClient>>,
    pub best_beacon_instance: Arc<AtomicUsize>,
}

impl<BeaconClient: BeaconClientTrait> MultiBeaconClient<BeaconClient> {
    pub fn new(beacon_clients: Vec<Arc<BeaconClient>>) -> Self {
        Self { beacon_clients, best_beacon_instance: Arc::new(AtomicUsize::new(0)) }
    }

    /// Returns a list of beacon clients, prioritized by the last successful response.
    ///
    /// The beacon client with the most recent successful response is placed at the
    /// beginning of the returned vector. All other clients maintain their original order.
    pub fn beacon_clients_by_last_response(&self) -> Vec<Arc<BeaconClient>> {
        let mut instances = self.beacon_clients.clone();
        let index = self.best_beacon_instance.load(Ordering::Relaxed);
        if index != 0 {
            instances.swap(0, index);
        }
        instances
    }

    /// Returns a list of beacon clients, prioritized by least recent usage.
    ///
    /// The beacon client with the most recent successful response is placed at the
    /// end of the returned vector, effectively reversing the order produced by
    /// `beacon_clients_by_last_response`.
    pub fn beacon_clients_by_least_used(&self) -> Vec<Arc<BeaconClient>> {
        let beacon_clients = self.beacon_clients_by_last_response();
        let mut instances = Vec::with_capacity(beacon_clients.len());

        for i in (0..beacon_clients.len()).rev() {
            instances.push(beacon_clients[i].clone());
        }
        instances
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<SignedBeaconBlock>,
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
            .map(|client| tokio::spawn(async move { client.sync_status().await }))
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

        for client in clients {
            let chan = chan.clone();
            tokio::spawn(async move {
                if let Err(err) = client.subscribe_to_head_events(chan).await {
                    tracing::error!("Failed to subscribe to head events: {err:?}");
                }
            });
        }
    }

    /// `subscribe_to_payload_attributes_events` subscribes to payload attributes events from all beacon nodes.
    ///
    /// This function swaps async tasks for all beacon clients. Therefore,
    /// a single payload event will be received multiple times, likely once for every beacon node.
    async fn subscribe_to_payload_attributes_events(&self, chan: Sender<PayloadAttributesEvent>) {
        let clients = self.beacon_clients_by_last_response();

        for client in clients {
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

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_state_validators(state_id.clone()).await {
                Ok(state_validators) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(state_validators);
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

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_proposer_duties(epoch).await {
                Ok(proposer_duties) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(proposer_duties);
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
    async fn publish_block<
        SignedBeaconBlock: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    >(
        &self,
        block: Arc<SignedBeaconBlock>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();

        let handles = clients
            .into_iter()
            .enumerate()
            .map(|(i, client)| {
                let block = block.clone();
                let broadcast_validation = broadcast_validation.clone();
                let fork = fork.clone();

                tokio::spawn(async move {
                    let res = client.publish_block(block, broadcast_validation, fork).await;
                    (i, res)
                })
            })
            .collect::<Vec<_>>();

        let results = join_all(handles).await;
        let mut last_error: Option<BeaconClientError> = None;

        for result in results {
            match result {
                Ok((i, res)) => {
                    match res {
                        // Should the block fail full validation, a separate success response code (202) is used to indicate that the block was successfully broadcast but failed integration.
                        // https://ethereum.github.io/beacon-APIs/?urls.primaryName=dev#/Beacon/publishBlock  // TODO: other codes can be block validation failed!
                        Ok(202) => {
                            return Err(BeaconClientError::BlockValidationFailed);
                        }
                        Ok(_) => {
                            self.best_beacon_instance.store(i, Ordering::Relaxed);
                            return Ok(());
                        }
                        Err(err) => {
                            last_error = Some(err);
                        }
                    }
                }
                Err(join_err) => {
                    tracing::error!("Tokio join error for publish_block: {join_err:?}")
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_genesis(&self) -> Result<GenesisDetails, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_genesis().await {
                Ok(genesis_info) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(genesis_info);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_spec(&self) -> Result<HashMap<String, String>, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_spec().await {
                Ok(spec) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(spec);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_fork_schedule(&self) -> Result<Vec<Fork>, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_fork_schedule().await {
                Ok(fork_schedule) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(fork_schedule);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_block<SignedBeaconBlock: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        block_id: BlockId,
    ) -> Result<SignedBeaconBlock, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_block(block_id.clone()).await {
                Ok(block) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(block);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    async fn get_randao(&self, id: StateId) -> Result<RandaoResponse, BeaconClientError> {
        let clients = self.beacon_clients_by_last_response();
        let mut last_error = None;

        for (i, client) in clients.into_iter().enumerate() {
            match client.get_randao(id.clone()).await {
                Ok(randao) => {
                    self.best_beacon_instance.store(i, Ordering::Relaxed);
                    return Ok(randao);
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }
}

#[cfg(test)]
mod multi_beacon_client_tests {
    use super::*;
    use crate::mock_beacon_client::MockBeaconClient;
    use helix_common::fork_info::ForkInfo;

    #[tokio::test]
    async fn test_beacon_clients_by_last_response() {
        let client1 = Arc::new(MockBeaconClient::new());
        let client2 = Arc::new(MockBeaconClient::new());
        let client3 = Arc::new(MockBeaconClient::new());
        let client4 = Arc::new(MockBeaconClient::new());
        let multi_client = MultiBeaconClient::new(vec![client1, client2, client3, client4]);

        multi_client.best_beacon_instance.store(2, Ordering::Relaxed);
        let clients = multi_client.beacon_clients_by_last_response();

        assert_eq!(Arc::ptr_eq(&clients[0], &multi_client.beacon_clients[2]), true);
        assert_eq!(Arc::ptr_eq(&clients[1], &multi_client.beacon_clients[1]), true);
        assert_eq!(Arc::ptr_eq(&clients[2], &multi_client.beacon_clients[0]), true);
        assert_eq!(Arc::ptr_eq(&clients[3], &multi_client.beacon_clients[3]), true);
    }

    #[tokio::test]
    async fn test_beacon_clients_by_least_used() {
        let client1 = Arc::new(MockBeaconClient::new());
        let client2 = Arc::new(MockBeaconClient::new());
        let client3 = Arc::new(MockBeaconClient::new());
        let client4 = Arc::new(MockBeaconClient::new());
        let multi_client = MultiBeaconClient::new(vec![client1, client2, client3, client4]);

        multi_client.best_beacon_instance.store(2, Ordering::Relaxed);
        let clients = multi_client.beacon_clients_by_least_used();

        assert_eq!(Arc::ptr_eq(&clients[0], &multi_client.beacon_clients[3]), true);
        assert_eq!(Arc::ptr_eq(&clients[1], &multi_client.beacon_clients[0]), true);
        assert_eq!(Arc::ptr_eq(&clients[2], &multi_client.beacon_clients[1]), true);
        assert_eq!(Arc::ptr_eq(&clients[3], &multi_client.beacon_clients[2]), true);
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
                Arc::new(()),
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
                Arc::new(()),
                Some(BroadcastValidation::default()),
                ethereum_consensus::Fork::Capella,
            )
            .await;

        assert!(matches!(result, Err(BeaconClientError::BlockValidationFailed)));
    }
}
