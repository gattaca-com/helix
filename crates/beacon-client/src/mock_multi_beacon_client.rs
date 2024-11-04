use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc,
};

use async_trait::async_trait;
use ethereum_consensus::{
    phase0::Validator,
    primitives::{BlsPublicKey, Root},
};
use helix_common::{
    beacon_api::PublishBlobsRequest, bellatrix::SimpleSerialize, ProposerDuty, ValidatorStatus,
    ValidatorSummary,
};
use tokio::sync::broadcast::Sender;

use crate::{
    error::BeaconClientError,
    types::{BroadcastValidation, HeadEventData, PayloadAttributesEvent, StateId, SyncStatus},
    MultiBeaconClientTrait,
};

#[derive(Default, Clone)]
pub struct MockMultiBeaconClient {
    subscribed_to_head_events: Arc<AtomicBool>,
    chan_head_events_capacity: Arc<AtomicUsize>,
    _chan: Option<Sender<HeadEventData>>,
    state_validators_has_been_read: Arc<AtomicBool>,
    proposer_duties_has_been_read: Arc<AtomicBool>,
}

impl MockMultiBeaconClient {
    pub fn new(
        subscribed_to_head_events: Arc<AtomicBool>,
        chan_head_events_capacity: Arc<AtomicUsize>,
        state_validators_has_been_read: Arc<AtomicBool>,
        proposer_duties_has_been_read: Arc<AtomicBool>,
    ) -> Self {
        Self {
            subscribed_to_head_events,
            chan_head_events_capacity,
            _chan: None,
            state_validators_has_been_read,
            proposer_duties_has_been_read,
        }
    }
}

#[async_trait]
impl MultiBeaconClientTrait for MockMultiBeaconClient {
    async fn best_sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        Ok(SyncStatus::default())
    }
    async fn subscribe_to_head_events(&self, chan: Sender<HeadEventData>) {
        // set the subscribed_to_head_events flag
        self.subscribed_to_head_events.store(true, std::sync::atomic::Ordering::Relaxed);

        // send a dummy event
        let head_event = HeadEventData {
            slot: 19,
            block: "test_block".to_string(),
            state: "test_state".to_string(),
        };
        let _ = chan.send(head_event);
        self.chan_head_events_capacity.store(chan.len(), std::sync::atomic::Ordering::Relaxed);

        // start a task that sets the number of events in the channel constantly
        let chan_head_events_capacity = self.chan_head_events_capacity.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                chan_head_events_capacity.store(chan.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    async fn subscribe_to_payload_attributes_events(&self, _chan: Sender<PayloadAttributesEvent>) {}
    async fn get_state_validators(
        &self,
        _state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        self.state_validators_has_been_read.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(vec![ValidatorSummary {
            index: 1,
            balance: 1,
            status: ValidatorStatus::Active,
            validator: Validator::default(),
        }])
    }
    async fn get_proposer_duties(
        &self,
        _epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError> {
        self.proposer_duties_has_been_read.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok((
            Root::default(),
            vec![ProposerDuty {
                public_key: BlsPublicKey::default(),
                validator_index: 1,
                slot: 19,
            }],
        ))
    }
    async fn publish_block<VersionedSignedProposal: SimpleSerialize + Send + Sync + 'static>(
        &self,
        _block: Arc<VersionedSignedProposal>,
        _broadcast_validation: Option<BroadcastValidation>,
        _fork: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        Ok(())
    }

    async fn publish_blobs(
        &self,
        _blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError> {
        Ok(0)
    }
}
