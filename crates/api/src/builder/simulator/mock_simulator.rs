use async_trait::async_trait;
use helix_common::{simulator::BlockSimError, BuilderInfo};

use crate::builder::{traits::BlockSimulator, BlockSimRequest};

#[derive(Clone, Default)]
pub struct MockSimulator {}

impl MockSimulator {
    pub fn new() -> Self {
        MockSimulator {}
    }
}

#[async_trait]
impl BlockSimulator for MockSimulator {
    async fn process_request(
        &self,
        _request: BlockSimRequest,
        _builder_info: &BuilderInfo,
        _is_top_bid: bool,
    ) -> Result<bool, BlockSimError> {
        Ok(true)
    }

    async fn is_synced(&self) -> Result<bool, BlockSimError> {
        Ok(true)
    }
}
