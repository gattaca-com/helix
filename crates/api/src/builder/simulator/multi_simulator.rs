use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use helix_common::{simulator::BlockSimError, BuilderInfo};
use tokio::{
    sync::{mpsc::Sender, RwLock},
    time::sleep,
};

use super::{traits::BlockSimulator, BlockSimRequest};
use crate::builder::DbInfo;

#[derive(Clone)]
pub struct MultiSimulator<B: BlockSimulator + Send + Sync> {
    pub simulators: Vec<B>,
    next_index: Arc<AtomicUsize>,
    enabled: Arc<RwLock<Vec<bool>>>,
}

impl<B: BlockSimulator + Send + Sync> MultiSimulator<B> {
    pub fn new(simulators: Vec<B>) -> Self {
        let num_simulators = simulators.len();
        Self {
            simulators,
            next_index: Arc::new(AtomicUsize::new(0)),
            enabled: Arc::new(RwLock::new(vec![true; num_simulators])),
        }
    }

    pub async fn start_sync_monitor(&self) {
        loop {
            for (i, simulator) in self.simulators.iter().enumerate() {
                let is_synced = simulator.is_synced().await.unwrap_or(false);
                let mut enabled = self.enabled.write().await;
                enabled[i] = is_synced;
            }
            sleep(Duration::from_secs(1)).await;
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
    ) -> Result<bool, BlockSimError> {
        let mut attempts = 0;

        loop {
            // Load balancing: round-robin selection
            let index = self
                .next_index
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                    Some((x + 1) % self.simulators.len())
                })
                .unwrap_or(0);

            // Check if the simulator is enabled
            let enabled = self.enabled.read().await;
            let simulator_enabled = enabled[index];
            if simulator_enabled {
                drop(enabled);
                let simulator = &self.simulators[index];

                // Process the request with the selected simulator
                return simulator
                    .process_request(
                        request.clone(),
                        builder_info,
                        is_top_bid,
                        sim_result_saver_sender,
                    )
                    .await
            }

            // If reached max attempts, return an error
            attempts += 1;
            if attempts >= self.simulators.len() {
                return Err(BlockSimError::NoSimulatorAvailable)
            }
        }
    }

    async fn is_synced(&self) -> Result<bool, BlockSimError> {
        // If any simulator is synced, then the multi-simulator is synced
        for simulator in &self.simulators {
            if simulator.is_synced().await? {
                return Ok(true)
            }
        }

        Ok(false)
    }
}
