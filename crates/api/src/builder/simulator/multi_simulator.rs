use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use helix_common::{metrics::SimulatorMetrics, simulator::BlockSimError, BuilderInfo};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use tokio::time::sleep;

use super::{optimistic_simulator::OptimisticSimulator, BlockMergeRequest, BlockSimRequest};

#[derive(Clone)]
pub struct MultiSimulator<A: Auctioneer + 'static, DB: DatabaseService + 'static> {
    pub simulators: Arc<Vec<OptimisticSimulator<A, DB>>>,
    next_index: Arc<AtomicUsize>,
    // never resized after init
    enabled: Arc<Vec<AtomicBool>>,
}

impl<A: Auctioneer + 'static, DB: DatabaseService + 'static> MultiSimulator<A, DB> {
    pub fn new(simulators: Vec<OptimisticSimulator<A, DB>>) -> Self {
        let enabled = vec![true; simulators.len()].into_iter().map(AtomicBool::new).collect();
        Self {
            simulators: Arc::new(simulators),
            next_index: Arc::new(AtomicUsize::new(0)),
            enabled: Arc::new(enabled),
        }
    }

    pub async fn start_sync_monitor(self) {
        loop {
            for (i, simulator) in self.simulators.iter().enumerate() {
                let is_synced = simulator.is_synced().await.unwrap_or(false);
                self.enabled[i].store(is_synced, Ordering::Relaxed);
                SimulatorMetrics::simulator_sync(simulator.endpoint(), is_synced);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    pub fn clone_for_async(&self) -> Self {
        self.clone()
    }

    pub async fn process_request(
        &self,
        request: BlockSimRequest,
        builder_info: &BuilderInfo,
        is_top_bid: bool,
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
            let simulator_enabled = self.enabled[index].load(Ordering::Relaxed);
            if simulator_enabled {
                let simulator = &self.simulators[index];

                // Process the request with the selected simulator
                return simulator.process_request(request.clone(), builder_info, is_top_bid).await;
            }

            // If reached max attempts, return an error
            attempts += 1;
            if attempts >= self.simulators.len() {
                return Err(BlockSimError::NoSimulatorAvailable);
            }
        }
    }

    pub async fn is_synced(&self) -> Result<bool, BlockSimError> {
        // If any simulator is synced, then the multi-simulator is synced
        for simulator in self.simulators.iter() {
            if simulator.is_synced().await? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub async fn process_merge_request(
        &self,
        request: BlockMergeRequest,
    ) -> Result<(), BlockSimError> {
        // TODO: send request to simulator
        Ok(())
    }
}
