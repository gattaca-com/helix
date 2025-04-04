use std::sync::Arc;

use helix_types::VersionedSignedProposal;

use crate::{error::BeaconClientError, types::BroadcastValidation};

#[derive(Default, Clone, Debug)]
pub struct MockBlockBroadcaster {}

impl MockBlockBroadcaster {
    pub async fn broadcast_block(
        &self,
        _block: Arc<VersionedSignedProposal>,
        _broadcast_validation: Option<BroadcastValidation>,
    ) -> Result<(), BeaconClientError> {
        Ok(())
    }

    pub fn identifier(&self) -> String {
        "MOCK-BLOCK-BROADCASTER".to_string()
    }
}
