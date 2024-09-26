use async_trait::async_trait;
use helix_common::{simulator::BlockSimError, BuilderInfo};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use crate::builder::{traits::BlockSimulator, BlockSimRequest, DbInfo};

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
        _sim_result_saver_sender: Sender<DbInfo>,
        _request_id: Uuid,
    ) -> Result<bool, BlockSimError> {
        Ok(true)
    }
}
