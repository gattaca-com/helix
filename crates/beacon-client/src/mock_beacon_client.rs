use std::sync::Arc;

use async_trait::async_trait;
use ethereum_consensus::{primitives::Root, ssz::prelude::*};
use tokio::sync::broadcast::Sender;

use helix_common::{beacon_api::PublishBlobsRequest, ProposerDuty, ValidatorSummary};

use crate::{
    error::BeaconClientError,
    traits::BeaconClientTrait,
    types::{BroadcastValidation, HeadEventData, PayloadAttributesEvent, StateId, SyncStatus},
};

#[derive(Clone, Default)]
pub struct MockBeaconClient {
    sync_status: SyncStatus,
    state_validators: Vec<ValidatorSummary>,
    proposer_duties: (Root, Vec<ProposerDuty>),
    publish_block_response_code: u16,
}

impl MockBeaconClient {
    pub fn new() -> Self {
        Self {
            sync_status: SyncStatus { head_slot: 10, sync_distance: 0, is_syncing: false },
            state_validators: Vec::new(),
            proposer_duties: (Root::default(), Vec::new()),
            publish_block_response_code: 200,
        }
    }

    pub fn with_sync_status(mut self, sync_status: SyncStatus) -> Self {
        self.sync_status = sync_status;
        self
    }

    pub fn with_state_validators(mut self, state_validators: Vec<ValidatorSummary>) -> Self {
        self.state_validators = state_validators;
        self
    }

    pub fn with_proposer_duties(mut self, proposer_duties: (Root, Vec<ProposerDuty>)) -> Self {
        self.proposer_duties = proposer_duties;
        self
    }

    pub fn with_publish_block_response_code(mut self, publish_block_response_code: u16) -> Self {
        self.publish_block_response_code = publish_block_response_code;
        self
    }
}

#[async_trait]
impl BeaconClientTrait for MockBeaconClient {
    async fn sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        Ok(self.sync_status.clone())
    }

    async fn current_slot(&self) -> Result<u64, BeaconClientError> {
        Ok(self.sync_status.head_slot)
    }

    async fn subscribe_to_head_events(
        &self,
        _chan: Sender<HeadEventData>,
    ) -> Result<(), BeaconClientError> {
        Ok(())
    }

    async fn subscribe_to_payload_attributes_events(
        &self,
        _chan: Sender<PayloadAttributesEvent>,
    ) -> Result<(), BeaconClientError> {
        Ok(())
    }

    async fn get_state_validators(
        &self,
        _state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        Ok(self.state_validators.clone())
    }

    async fn get_proposer_duties(
        &self,
        _epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError> {
        Ok(self.proposer_duties.clone())
    }

    fn get_uri(&self) -> String {
        "test_uri".to_string()
    }

    async fn publish_block<SignedBeaconBlock: Send + Sync + SimpleSerialize>(
        &self,
        _block: Arc<SignedBeaconBlock>,
        _broadcast_validation: Option<BroadcastValidation>,
        _fork: ethereum_consensus::Fork,
    ) -> Result<u16, BeaconClientError> {
        Ok(self.publish_block_response_code)
    }

    async fn publish_blobs(
        &self,
        _blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError> {
        Ok(self.publish_block_response_code)
    }
}
