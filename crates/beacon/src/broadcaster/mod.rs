use std::sync::Arc;

use helix_types::{ForkName, VersionedSignedProposal};

use crate::{beacon_client::BeaconClient, error::BeaconClientError, types::BroadcastValidation};

pub enum BlockBroadcaster {
    BeaconClient(BeaconClient),
}

impl BlockBroadcaster {
    pub async fn broadcast_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        consensus_version: ForkName,
    ) -> Result<(), BeaconClientError> {
        match self {
            BlockBroadcaster::BeaconClient(b) => {
                b.broadcast_block(block, broadcast_validation, consensus_version).await
            }
        }
    }

    pub fn identifier(&self) -> String {
        match self {
            BlockBroadcaster::BeaconClient(b) => b.identifier(),
        }
    }
}
