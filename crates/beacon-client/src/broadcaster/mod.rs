pub mod fiber_broadcaster;
pub mod mock_block_broadcaster;

use ethereum_consensus::types::mainnet::SignedBeaconBlock;
use std::sync::Arc;

use crate::{
    beacon_client::BeaconClient,
    error::BeaconClientError, fiber_broadcaster::FiberBroadcaster,
    mock_block_broadcaster::MockBlockBroadcaster, types::BroadcastValidation,
};

pub enum BlockBroadcaster {
    Fiber(FiberBroadcaster),
    BeaconClient(BeaconClient),
    Mock(MockBlockBroadcaster),
}

impl BlockBroadcaster {
    pub async fn broadcast_block(
        &self,
        block: Arc<SignedBeaconBlock>,
        broadcast_validation: Option<BroadcastValidation>,
        consensus_version: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        match self {
            BlockBroadcaster::Fiber(f) => {
                f.broadcast_block(block, broadcast_validation, consensus_version).await
            }
            BlockBroadcaster::BeaconClient(b) => {
                b.broadcast_block(block, broadcast_validation, consensus_version).await
            }
            BlockBroadcaster::Mock(b) => {
                b.broadcast_block(block, broadcast_validation, consensus_version).await
            }
        }
    }

    pub fn identifier(&self) -> String {
        match self {
            BlockBroadcaster::Fiber(f) => f.identifier(),
            BlockBroadcaster::BeaconClient(b) => b.identifier(),
            BlockBroadcaster::Mock(b) => b.identifier(),
        }
    }
}
