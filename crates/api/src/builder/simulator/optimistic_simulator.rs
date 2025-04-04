use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::B256;
use async_trait::async_trait;
use helix_common::{metrics::SimulatorMetrics, simulator::BlockSimError, task, BuilderInfo};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::BlsPublicKey;
use reqwest::Client;
use tokio::{
    sync::{mpsc::Sender, RwLock},
    time::sleep,
};
use tracing::{debug, error, warn, Instrument};

use crate::builder::{
    rpc_simulator::RpcSimulator, traits::BlockSimulator, BlockSimRequest, DbInfo,
};

/// OptimisticSimulator is responsible for running simulations optimistically or synchronously based
/// on the builder's status.
#[derive(Clone)]
pub struct OptimisticSimulator<A: Auctioneer + 'static, DB: DatabaseService + 'static> {
    simulator: Arc<RpcSimulator>,
    auctioneer: Arc<A>,
    db: Arc<DB>,
    /// The `failsafe_triggered` flag serves as a circuit breaker for the optimistic simulation
    /// process.
    ///
    /// If a simulation error occurs and the auctioneer fails to update the builder status, this
    /// flag will be set to `true`. Once triggered, the system will halt all optimistic
    /// simulations.
    failsafe_triggered: Arc<RwLock<bool>>,
    optimistic_state: Arc<PauseState>,
}

impl<A: Auctioneer + 'static, DB: DatabaseService + 'static> OptimisticSimulator<A, DB> {
    pub fn new(auctioneer: Arc<A>, db: Arc<DB>, http: Client, endpoint: String) -> Self {
        let simulator = Arc::new(RpcSimulator::new(http, endpoint));
        let failsafe_triggered = Arc::new(RwLock::new(false));
        let optimistic_state = Arc::new(PauseState::new(Duration::from_secs(60)));
        Self { simulator, auctioneer, db, failsafe_triggered, optimistic_state }
    }

    /// This is a lightweight operation as all params are references.
    pub fn clone_for_async(&self) -> Self {
        Self {
            simulator: self.simulator.clone(),
            auctioneer: self.auctioneer.clone(),
            db: self.db.clone(),
            failsafe_triggered: self.failsafe_triggered.clone(),
            optimistic_state: self.optimistic_state.clone(),
        }
    }

    /// Handle simulation of request.
    ///
    /// If the simulation fails and the builder is optimistic, it will be demoted.
    /// The simulation result will be written to the db in `self.simulator.process_request`
    async fn handle_simulation(
        &self,
        request: BlockSimRequest,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
        builder_info: BuilderInfo,
    ) -> Result<(), BlockSimError> {
        if let Err(err) = self
            .simulator
            .process_request(request.clone(), &builder_info, is_top_bid, sim_result_saver_sender)
            .await
        {
            if builder_info.is_optimistic {
                if err.is_aleady_known() {
                    warn!(
                        builder=%request.message.builder_pubkey,
                        block_hash=%request.execution_payload.block_hash(),
                        "Block already known. Skipping demotion"
                    );
                    return Ok(());
                }

                if err.is_temporary() {
                    self.optimistic_state.pause();

                    // Pause optimistic simulations until the node is synced
                    warn!(
                        builder=%request.message.builder_pubkey,
                        block_hash=%request.execution_payload.block_hash(),
                        err=%err,
                        "Block simulation resulted in a temporary error. Pausing optimistic simulations...",
                    );
                    return Err(err);
                }

                warn!(
                    builder=%request.message.builder_pubkey,
                    block_hash=%request.execution_payload.block_hash(),
                    err=%err,
                    "Block simulation resulted in an error. Demoting builder...",
                );
                self.demote_builder_due_to_error(
                    &request.message.builder_pubkey,
                    &request.execution_payload.block_hash().0,
                    err.to_string(),
                )
                .await;
            }
            return Err(err);
        }

        Ok(())
    }

    /// Demotes a builder in the `auctioneer` and `db`.
    ///
    /// If demotion fails, the failsafe is triggered to halt all optimistic simulations.
    async fn demote_builder_due_to_error(
        &self,
        builder_public_key: &BlsPublicKey,
        block_hash: &B256,
        reason: String,
    ) {
        SimulatorMetrics::demotion_count();

        if let Err(err) = self.auctioneer.demote_builder(builder_public_key).await {
            *self.failsafe_triggered.write().await = true;
            error!(
                builder=%builder_public_key,
                err=%err,
                "Failed to demote builder in auctioneer"
            );
        }

        if let Err(err) = self.db.db_demote_builder(builder_public_key, block_hash, reason).await {
            *self.failsafe_triggered.write().await = true;
            error!(
                builder=%builder_public_key,
                err=%err,
                "Failed to demote builder in database"
            );
        }
    }

    /// Will return true if:
    /// - The failsafe hasn't been triggered.
    /// - The builder has optimistic relaying enabled.
    /// - The builder collateral is greater than the block value.
    /// - The proposer preferences do not have regional filtering enabled or the builder is
    ///   optimistic for regional filtering.
    async fn should_process_optimistically(
        &self,
        request: &BlockSimRequest,
        builder_info: &BuilderInfo,
    ) -> bool {
        if builder_info.is_optimistic && request.message.value <= builder_info.collateral {
            if request.proposer_preferences.filtering.is_regional()
                && !builder_info.can_process_regional_slot_optimistically()
            {
                return false;
            }

            if *self.failsafe_triggered.read().await {
                warn!(
                    builder=%request.message.builder_pubkey,
                    block_hash=%request.execution_payload.block_hash(),
                    "Failsafe triggered. Skipping optimistic simulation"
                );
                return false;
            }

            if self.optimistic_state.is_paused() {
                warn!(
                    builder=%request.message.builder_pubkey,
                    block_hash=%request.execution_payload.block_hash(),
                    "Optimistic simulation paused. Skipping simulation"
                );
                return false;
            }

            return true;
        }

        false
    }
}

#[async_trait]
impl<A: Auctioneer, DB: DatabaseService> BlockSimulator for OptimisticSimulator<A, DB> {
    async fn process_request(
        &self,
        request: BlockSimRequest,
        builder_info: &BuilderInfo,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
    ) -> Result<bool, BlockSimError> {
        if self.should_process_optimistically(&request, builder_info).await {
            SimulatorMetrics::sim_count(true);

            debug!(
                block_hash=%request.execution_payload.block_hash(),
                "optimistically processing request"
            );

            let cloned_self = self.clone_for_async();
            let builder_info = builder_info.clone();
            let sender_clone = sim_result_saver_sender.clone();
            task::spawn(file!(), line!(), {
                async move {
                    if let Err(e) = cloned_self
                        .handle_simulation(request, is_top_bid, sender_clone, builder_info)
                        .await
                    {
                        error!("Simulation failed: {:?}", e);
                    }
                }
                .in_current_span()
            });

            Ok(true)
        } else {
            SimulatorMetrics::sim_count(false);

            debug!(

                block_hash=?request.execution_payload.block_hash(),
                block_parent_hash=?request.execution_payload.parent_hash(),
                block_number=%request.execution_payload.block_number(),
                request=?request.message,
                "processing simulation synchronously"
            );
            self.handle_simulation(
                request,
                is_top_bid,
                sim_result_saver_sender,
                builder_info.clone(),
            )
            .await
            .map(|_| false)
        }
    }

    async fn is_synced(&self) -> Result<bool, BlockSimError> {
        self.simulator.is_synced().await
    }
}

#[derive(Clone)]
pub struct PauseState {
    /// Indicates whether we are currently "paused" (true) or not (false).
    is_paused: Arc<AtomicBool>,
    /// How long the pause lasts once triggered.
    duration: Duration,
}

impl PauseState {
    /// Create a new `PauseState` with a specified cooldown or “pause” duration.
    pub fn new(duration: Duration) -> Self {
        Self { is_paused: Arc::new(AtomicBool::new(false)), duration }
    }

    /// Trigger a pause. If we are not already paused, we set it to paused
    /// and spawn a background task to un-pause after `self.duration`.
    ///
    /// If already paused, this does nothing.
    pub fn pause(&self) {
        if self.is_paused.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
        {
            // We just transitioned from false -> true. Spawn a task to
            // reset to false after `duration`.
            let duration = self.duration;
            let this = self.clone();
            task::spawn(file!(), line!(), async move {
                sleep(duration).await;
                this.is_paused.store(false, Ordering::SeqCst);
            });
        }
    }

    /// Returns whether we are currently paused.
    pub fn is_paused(&self) -> bool {
        self.is_paused.load(Ordering::SeqCst)
    }
}
