use std::sync::Arc;

use async_trait::async_trait;
use ethereum_consensus::primitives::{BlsPublicKey, Hash32};
use reqwest::Client;
use tokio::sync::{mpsc::Sender, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use helix_common::{simulator::BlockSimError, BuilderInfo};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;

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
}

impl<A: Auctioneer + 'static, DB: DatabaseService + 'static> OptimisticSimulator<A, DB> {
    pub fn new(auctioneer: Arc<A>, db: Arc<DB>, http: Client, endpoint: String) -> Self {
        let simulator = Arc::new(RpcSimulator::new(http, endpoint));
        let failsafe_triggered = Arc::new(RwLock::new(false));
        Self { simulator, auctioneer, db, failsafe_triggered }
    }

    /// This is a lightweight operation as all params are references.
    pub fn clone_for_async(&self) -> Self {
        Self {
            simulator: self.simulator.clone(),
            auctioneer: self.auctioneer.clone(),
            db: self.db.clone(),
            failsafe_triggered: self.failsafe_triggered.clone(),
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
        request_id: Uuid,
    ) -> Result<(), BlockSimError> {
        if let Err(err) = self
            .simulator
            .process_request(
                request.clone(),
                &builder_info,
                is_top_bid,
                sim_result_saver_sender,
                request_id,
            )
            .await
        {
            if let BlockSimError::BlockValidationFailed(_) = err {
                if builder_info.is_optimistic {
                    if err.is_severe() {
                        warn!(
                            request_id=%request_id,
                            builder=%request.message.builder_public_key,
                            block_hash=%request.execution_payload.block_hash(),
                            err=%err,
                            "Block simulation resulted in an error. Demoting builder...",
                        );
                        self.demote_builder_due_to_error(
                            &request.message.builder_public_key,
                            request.execution_payload.block_hash(),
                            err.to_string(),
                        )
                        .await;
                    } else {
                        warn!(
                            request_id=%request_id,
                            builder=%request.message.builder_public_key,
                            block_hash=%request.execution_payload.block_hash(),
                            err=%err,
                            "Block simulation resulted in a non-severe error. NOT demoting builder...",
                        );
                    }
                }
            }
            return Err(err)
        }

        Ok(())
    }

    /// Demotes a builder in the `auctioneer` and `db`.
    ///
    /// If demotion fails, the failsafe is triggered to halt all optimistic simulations.
    async fn demote_builder_due_to_error(
        &self,
        builder_public_key: &BlsPublicKey,
        block_hash: &Hash32,
        reason: String,
    ) {
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
    /// - The proposer preferences do not have regional filtering enabled.
    async fn should_process_optimistically(
        &self,
        request: &BlockSimRequest,
        builder_info: &BuilderInfo,
    ) -> bool {
        if builder_info.is_optimistic && request.message.value <= builder_info.collateral {
            if *self.failsafe_triggered.read().await {
                warn!(
                    builder=%request.message.builder_public_key,
                    block_hash=%request.execution_payload.block_hash(),
                    "Failsafe triggered. Skipping optimistic simulation"
                );
                return false
            }
            return true
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
        request_id: Uuid,
    ) -> Result<bool, BlockSimError> {
        if self.should_process_optimistically(&request, builder_info).await {
            info!(
                request_id=%request_id,
                block_hash=%request.execution_payload.block_hash(),
                "optimistically processing request"
            );

            let cloned_self = self.clone_for_async();
            let builder_info = builder_info.clone();
            tokio::spawn(async move {
                cloned_self
                    .handle_simulation(
                        request,
                        is_top_bid,
                        sim_result_saver_sender,
                        builder_info,
                        request_id,
                    )
                    .await
            });

            Ok(true)
        } else {
            info!(
                request_id=%request_id,
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
                request_id,
            )
            .await
            .map(|_| false)
        }
    }
}
