use axum::async_trait;
use helix_common::{simulator::BlockSimError, BuilderInfo};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::builder::DbInfo;

use super::{traits::BlockSimulator, BlockSimRequest};

#[derive(Clone)]
pub struct MultiSimulator<B: BlockSimulator + Send + Sync> {
    pub simulators: Vec<B>,
    next_index: Arc<AtomicUsize>,
}

impl<B: BlockSimulator + Send + Sync> MultiSimulator<B> {
    pub fn new(simulators: Vec<B>) -> Self {
        Self {
            simulators,
            next_index: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn clone_for_async(&self) -> Self {
        self.clone()
    }
}

#[async_trait]
impl<B: BlockSimulator + Send + Sync> BlockSimulator for MultiSimulator<B> {
    async fn process_request(
        &self,
        request: BlockSimRequest,
        builder_info: &BuilderInfo,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
        request_id: Uuid,
    ) -> Result<bool, BlockSimError> {
        // Load balancing: round-robin selection
        let index = self
            .next_index
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                Some((x + 1) % self.simulators.len())
            })
            .unwrap_or(0);

        let simulator = &self.simulators[index];

        // Process the request with the selected simulator
        simulator
            .process_request(
                request,
                builder_info,
                is_top_bid,
                sim_result_saver_sender,
                request_id,
            )
            .await
    }
}
