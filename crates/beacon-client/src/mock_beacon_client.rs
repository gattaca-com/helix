use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ethereum_consensus::{phase0::Fork, primitives::Root, ssz::prelude::*};
use tokio::sync::mpsc::Sender;

use helix_common::{ProposerDuty, ValidatorSummary};

use crate::{
    error::BeaconClientError,
    traits::BeaconClientTrait,
    types::{
        BlockId, BroadcastValidation, GenesisDetails, HeadEventData, PayloadAttributesEvent,
        RandaoResponse, StateId, SyncStatus,
    },
};

#[derive(Clone, Default)]
pub struct MockBeaconClient {
    sync_status: SyncStatus,
    state_validators: Vec<ValidatorSummary>,
    proposer_duties: (Root, Vec<ProposerDuty>),
    genesis: GenesisDetails,
    spec: HashMap<String, String>,
    fork_schedule: Vec<Fork>,
    publish_block_response_code: u16,
    randao: RandaoResponse,
}

impl MockBeaconClient {
    pub fn new() -> Self {
        Self {
            sync_status: SyncStatus { head_slot: 10, sync_distance: 0, is_syncing: false },
            state_validators: Vec::new(),
            proposer_duties: (Root::default(), Vec::new()),
            genesis: GenesisDetails::default(),
            spec: HashMap::new(),
            fork_schedule: Vec::new(),
            publish_block_response_code: 200,
            randao: RandaoResponse { randao: ByteVector::<32>::default() },
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

    pub fn with_genesis(mut self, genesis: GenesisDetails) -> Self {
        self.genesis = genesis;
        self
    }

    pub fn with_spec(mut self, spec: HashMap<String, String>) -> Self {
        self.spec = spec;
        self
    }

    pub fn with_fork_schedule(mut self, fork_schedule: Vec<Fork>) -> Self {
        self.fork_schedule = fork_schedule;
        self
    }

    pub fn with_publish_block_response_code(mut self, publish_block_response_code: u16) -> Self {
        self.publish_block_response_code = publish_block_response_code;
        self
    }

    pub fn with_randao(mut self, randao: RandaoResponse) -> Self {
        self.randao = randao;
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

    async fn publish_block<
        SignedBeaconBlock: Send + Sync + SimpleSerialize,
    >(
        &self,
        _block: Arc<SignedBeaconBlock>,
        _broadcast_validation: Option<BroadcastValidation>,
        _fork: ethereum_consensus::Fork,
    ) -> Result<u16, BeaconClientError> {
        Ok(self.publish_block_response_code)
    }

    async fn get_genesis(&self) -> Result<GenesisDetails, BeaconClientError> {
        Ok(self.genesis.clone())
    }

    async fn get_spec(&self) -> Result<HashMap<String, String>, BeaconClientError> {
        Ok(self.spec.clone())
    }

    async fn get_fork_schedule(&self) -> Result<Vec<Fork>, BeaconClientError> {
        Ok(self.fork_schedule.clone())
    }

    async fn get_block<SignedBeaconBlock: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        _block_id: BlockId,
    ) -> Result<SignedBeaconBlock, BeaconClientError> {
        Err(BeaconClientError::BeaconNodeSyncing)
    }

    async fn get_randao(&self, _id: StateId) -> Result<RandaoResponse, BeaconClientError> {
        Ok(self.randao.clone())
    }
}
