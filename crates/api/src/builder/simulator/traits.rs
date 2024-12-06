use async_trait::async_trait;
use helix_common::{simulator::BlockSimError, BuilderInfo};
use tokio::sync::mpsc::Sender;

use crate::builder::{BlockSimRequest, DbInfo};

// Probably need access to db to store demotions + builder registry to track prios
#[async_trait]
pub trait BlockSimulator: Send + Sync + Clone {
    async fn process_request(
        &self,
        request: BlockSimRequest,
        builder_info: &BuilderInfo,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
    ) -> Result<bool, BlockSimError>;
}
