use std::sync::Arc;

use ethereum_consensus::Fork;
use helix_common::signed_proposal::VersionedSignedProposal;

use crate::{error::BeaconClientError, types::BroadcastValidation};

#[derive(Default, Clone, Debug)]
pub struct MockBlockBroadcaster {}

impl MockBlockBroadcaster {
    pub async fn broadcast_block(
        &self,
        _block: Arc<VersionedSignedProposal>,
        _broadcast_validation: Option<BroadcastValidation>,
        _consensus_version: Fork,
    ) -> Result<(), BeaconClientError> {
        Ok(())
    }

    pub fn identifier(&self) -> String {
        "MOCK-BLOCK-BROADCASTER".to_string()
    }
}
