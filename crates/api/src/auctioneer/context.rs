use std::sync::Arc;

use alloy_primitives::U256;
use helix_common::{
    bid_submission::BidSubmission, chain_info::ChainInfo, metrics::SimulatorMetrics, BuilderInfo,
    RelayConfig,
};
use helix_database::DatabaseService;
use helix_types::BlsPublicKeyBytes;
use rustc_hash::FxHashMap;
use tracing::{error, warn};

use crate::{
    auctioneer::{
        simulator::manager::{SimulationResult, SimulatorManager},
        types::PendingPayload,
    },
    Api,
};

pub struct Context<A: Api> {
    pub chain_info: ChainInfo,
    pub builder_infos: FxHashMap<BlsPublicKeyBytes, BuilderInfo>,
    pub unknown_builder_info: BuilderInfo,
    pub sim_manager: SimulatorManager,
    // TODO: on transition to new slot, send ProposerApiError::NoExecutionPayloadFound for this
    // TODO: move to sorting data
    pub pending_payload: Option<PendingPayload>,
    // flags
    // failsafe_triggered + accept_optimistic
    pub can_process_optimistic: bool,
    pub db: Arc<A::DatabaseService>,
    pub config: RelayConfig,
}
impl<A: Api> Context<A> {
    pub fn new(
        chain_info: ChainInfo,
        config: RelayConfig,
        sim_manager: SimulatorManager,
        db: Arc<A::DatabaseService>,
    ) -> Self {
        let unknown_builder_info = BuilderInfo {
            collateral: U256::ZERO,
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };

        // TODO: sync this from housekeeper
        let builder_infos = FxHashMap::with_capacity_and_hasher(200, Default::default());

        Self {
            chain_info,
            builder_infos,
            unknown_builder_info,
            sim_manager,
            pending_payload: None,
            can_process_optimistic: true,
            db,
            config,
        }
    }

    pub fn handle_simulation_result(&mut self, result: SimulationResult) {
        self.sim_manager.handle_task_response(result.id, result.paused_until);

        let builder = *result.submission.builder_public_key();
        let block_hash = *result.submission.block_hash();

        if let Err(err) = result.result.as_ref() {
            if let Some(builder_info) = self.builder_infos.get_mut(&builder) {
                if builder_info.is_optimistic {
                    if err.is_demotable() {
                        warn!(%builder, %block_hash, %err, "Block simulation resulted in an error. Demoting builder...");

                        SimulatorMetrics::demotion_count();

                        builder_info.is_optimistic = false;
                        builder_info.is_optimistic_for_regional_filtering = false;

                        let db = self.db.clone();
                        let reason = err.to_string();
                        let bid_slot = result.submission.slot();

                        tokio::spawn(async move {
                            if let Err(err) = db
                                .db_demote_builder(bid_slot.as_u64(), &builder, &block_hash, reason)
                                .await
                            {
                                // TODO:
                                // self.failsafe_triggered.store(true, Ordering::Relaxed);
                                error!(%builder, %err, %block_hash, "failed to demote builder in database");
                            }
                        });
                    } else {
                        warn!(%err, %builder, %block_hash, "failed simulation with known error, skipping demotion");
                    }
                }
            };
        }
    }

    pub fn clear(&mut self) {}
}
