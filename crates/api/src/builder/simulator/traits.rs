use async_trait::async_trait;
use helix_common::simulator::BlockSimError;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use crate::builder::{DbInfo, BlockSimRequest};

// Probably need access to db to store demotions + builder registry to track prios
#[async_trait]
pub trait BlockSimulator: Send + Sync + Clone {
    async fn process_request(
        &self,
        request: BlockSimRequest,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
        request_id: Uuid,
    ) -> Result<bool, BlockSimError>;
}
