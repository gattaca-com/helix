use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use helix_common::{beacon_api::PublishBlobsRequest, ProposerDuty, ValidatorSummary};
use helix_types::ForkName;
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::broadcast::Sender;

use crate::{
    error::BeaconClientError,
    types::{BroadcastValidation, HeadEventData, PayloadAttributesEvent, StateId, SyncStatus},
};
use ssz::Encode;

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
    ) -> Result<(B256, Vec<ProposerDuty>), BeaconClientError>;
    async fn publish_block<VersionedSignedProposal: Send + Sync + Encode>(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<u16, BeaconClientError>;
    async fn publish_blobs(
        &self,
        blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError>;
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
    ) -> Result<(B256, Vec<ProposerDuty>), BeaconClientError>;
    async fn publish_block<
        VersionedSignedProposal: Serialize + DeserializeOwned + Send + Sync + 'static + Encode,
    >(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<(), BeaconClientError>;
    async fn publish_blobs(
        &self,
        blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError>;
}
