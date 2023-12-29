use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ethereum_consensus::{phase0::Fork, primitives::Root};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::mpsc::Sender;

use helix_common::{ProposerDuty, ValidatorSummary};

use crate::{
    error::BeaconClientError,
    types::{
        BlockId, BroadcastValidation, GenesisDetails, HeadEventData, PayloadAttributesEvent,
        RandaoResponse, StateId, SyncStatus,
    },
};

#[async_trait]
pub trait BeaconClientTrait: Send + Sync + Clone {
    async fn sync_status(&self) -> Result<SyncStatus, BeaconClientError>;
    async fn current_slot(&self) -> Result<u64, BeaconClientError>;

    async fn subscribe_to_head_events(
        &self,
        chan: Sender<HeadEventData>,
    ) -> Result<(), BeaconClientError>;
    async fn subscribe_to_payload_attributes_events(
        &self,
        chan: Sender<PayloadAttributesEvent>,
    ) -> Result<(), BeaconClientError>;

    async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError>;
    async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError>;
    async fn publish_block<SignedBeaconBlock: Serialize + DeserializeOwned + Send + Sync>(
        &self,
        block: Arc<SignedBeaconBlock>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<u16, BeaconClientError>;
    async fn get_genesis(&self) -> Result<GenesisDetails, BeaconClientError>;
    async fn get_spec(&self) -> Result<HashMap<String, String>, BeaconClientError>;
    async fn get_fork_schedule(&self) -> Result<Vec<Fork>, BeaconClientError>;
    async fn get_block<SignedBeaconBlock: Serialize + DeserializeOwned>(
        &self,
        block_id: BlockId,
    ) -> Result<SignedBeaconBlock, BeaconClientError>;
    async fn get_randao(&self, id: StateId) -> Result<RandaoResponse, BeaconClientError>;
    fn get_uri(&self) -> String;
}

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait MultiBeaconClientTrait: Send + Sync + Clone {
    async fn best_sync_status(&self) -> Result<SyncStatus, BeaconClientError>;
    async fn subscribe_to_head_events(&self, chan: Sender<HeadEventData>);
    async fn subscribe_to_payload_attributes_events(&self, chan: Sender<PayloadAttributesEvent>);
    async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError>;
    async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError>;
    async fn publish_block<
        SignedBeaconBlock: Serialize + DeserializeOwned + Send + Sync + 'static,
    >(
        &self,
        block: Arc<SignedBeaconBlock>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError>;
    async fn get_genesis(&self) -> Result<GenesisDetails, BeaconClientError>;
    async fn get_spec(&self) -> Result<HashMap<String, String>, BeaconClientError>;
    async fn get_fork_schedule(&self) -> Result<Vec<Fork>, BeaconClientError>;
    async fn get_block<SignedBeaconBlock: Serialize + DeserializeOwned>(
        &self,
        block_id: BlockId,
    ) -> Result<SignedBeaconBlock, BeaconClientError>;
    async fn get_randao(&self, id: StateId) -> Result<RandaoResponse, BeaconClientError>;
}
