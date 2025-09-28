#![allow(unused)]

mod bid_sorter;
mod get_header;
mod get_payload;
mod sim_manager;
mod submit_block;
mod validation;
pub mod worker;

use std::{
    collections::BTreeMap,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256};
use helix_beacon::{multi_beacon_client::MultiBeaconClient, types::BroadcastValidation};
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
    },
    bid_submission::{BidSubmission, OptimisticVersion},
    chain_info::ChainInfo,
    metrics::{SimulatorMetrics, HYDRATION_CACHE_HITS},
    simulator::BlockSimError,
    utils::utcnow_ns,
    BuilderInfo, GetPayloadTrace, RelayConfig, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{
    BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ForkName, GetPayloadResponse,
    HydrationCache, PayloadAndBlobs, SignedBidSubmission, SignedBlindedBeaconBlock,
    SignedBuilderBid, Slot, VersionedSignedProposal,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::{
    builder::{
        error::BuilderApiError,
        simulator_2::{
            bid_sorter::{BidSorter, BidSorterMessage},
            sim_manager::{SimulationResult, SimulatorManager},
            worker::{
                GetHeaderResult, GetPayloadResult, GetPayloadResultData, Submission,
                SubmissionResult,
            },
        },
        BlockSimRequest, SimReponse, SimulatorRequest,
    },
    gossiper::{
        grpc_gossiper::GrpcGossiperClientManager,
        types::{BroadcastGetPayloadParams, BroadcastPayloadParams},
    },
    proposer::{GetHeaderParams, ProposerApiError},
    Api,
};

pub struct Auctioneer<A: Api> {
    ctx: Context<A>,
    state: State,
}

impl<A: Api> Auctioneer<A> {
    fn run(mut self, rx: crossbeam_channel::Receiver<Event>) {
        loop {
            for evt in rx.try_iter() {
                self.state.step(evt, &mut self.ctx);
            }

            self.state.tick();
        }
    }
}

struct Context<A: Api> {
    chain_info: ChainInfo,
    builder_info: FxHashMap<BlsPublicKeyBytes, BuilderInfo>,
    unknown_builder_info: BuilderInfo,
    simulator_manager: SimulatorManager,
    // TODO: on transition to new slot, send ProposerApiError::NoExecutionPayloadFound for this
    pending_payloads: Option<PendingPayload>,
    // flags
    // failsafe_triggered + accept_optimistic
    can_process_optimistic: bool,
    db: Arc<A::DatabaseService>,
    config: RelayConfig,
}

struct PendingPayload {
    block_hash: B256,
    blinded: SignedBlindedBeaconBlock,
    res_tx: oneshot::Sender<GetPayloadResult>,
    retry_at: Instant,
}

impl<A: Api> Context<A> {
    fn handle_simulation_result(&mut self, result: SimulationResult) {
        self.simulator_manager.handle_task_response(result.id, result.paused_until);

        let builder = *result.submission.builder_public_key();
        let block_hash = *result.submission.block_hash();

        if let Err(err) = result.result.as_ref() {
            if let Some(builder_info) = self.builder_info.get_mut(&builder) {
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

    fn clear(&mut self) {}
}

struct SortingData {
    slot: SlotContext,
    sort: BidSorter,
    seen_block_hashes: FxHashSet<B256>,
    sequence: FxHashMap<BlsPublicKeyBytes, u64>,
    hydration_cache: HydrationCache,
    payloads: FxHashMap<B256, Arc<PayloadAndBlobs>>,
    inclusion_list: Option<InclusionListWithMetadata>,
}

impl SlotContext {
    pub fn proposer_pubkey(&self) -> &BlsPublicKeyBytes {
        &self.registration_data.entry.registration.message.pubkey
    }
}

enum State {
    /// Next proposer is registered and we are processing builder bids
    Sorting(SortingData),
    /// Processing get_payload, broadcasting block
    Broadcasting { slot_ctx: SlotContext, block_hash: B256 },
    /// Next proposer is not registered, reject most calls
    Slot { slot: Slot },
}

pub enum Event {
    Submission {
        submission: Submission,
        withdrawals_root: B256,
        sequence: Option<u64>,
        trace: SubmissionTrace,
        res_tx: oneshot::Sender<SubmissionResult>,
    },
    /// Assume already some validation (so we don't have to wait here)
    /// timing games already done
    GetHeader {
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    },
    // Receive multiple of these potentially, assume some light validation
    GetPayload {
        block_hash: B256,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
    },
    GossipPayload(BroadcastPayloadParams),
    NewSlot,
    SimResult(SimulationResult),
    SimulatorSync {
        id: usize,
        is_synced: bool,
    },
}

impl State {
    // TODO: state transitions
    fn step<A: Api>(&mut self, event: Event, ctx: &mut Context<A>) {
        match (self, event) {
            ///////////// LIFECYCLE EVENTS (ALWAYS VALID) /////////////

            // new slot
            (_, Event::NewSlot) => {
                ctx.clear();
            }

            // simulator sync status
            (_, Event::SimulatorSync { id, is_synced }) => {
                ctx.simulator_manager.handle_sync_status(id, is_synced);
            }

            // late sim results
            (State::Broadcasting { .. } | State::Slot { .. }, Event::SimResult(result)) => {
                ctx.handle_simulation_result(result);
            }

            ///////////// VALID STATES / EVENTS /////////////

            // submit block
            (
                State::Sorting(sorting),
                Event::Submission { submission, withdrawals_root, sequence, mut trace, res_tx },
            ) => sorting.handle_submission(
                submission,
                withdrawals_root,
                sequence,
                trace,
                ctx,
                res_tx,
            ),

            // get header
            (State::Sorting(sorting), Event::GetHeader { params, res_tx }) => {
                sorting.handle_get_header(params, res_tx)
            }

            // get paylaod
            (State::Sorting(sorting), Event::GetPayload { blinded, block_hash, trace, res_tx }) => {
                sorting.handle_get_payload(block_hash, blinded, trace, res_tx, ctx);
            }

            // sim result
            (State::Sorting(sorting), Event::SimResult(result)) => {
                sorting.handle_simulated_submission(&result);
                ctx.handle_simulation_result(result);
            }

            (State::Sorting(sorting), Event::GossipPayload(payload)) => {
                sorting.handle_gossip_payload(payload);
            }

            ///////////// INVALID STATES / EVENTS /////////////

            // late submission
            (
                State::Broadcasting { slot_ctx, .. },
                Event::Submission { submission, res_tx, .. },
            ) => {
                let _ = res_tx.send(Err(BuilderApiError::DeliveringPayload {
                    bid_slot: submission.bid_slot(),
                    delivering: slot_ctx.bid_slot.as_u64(),
                }));
            }

            // late get_header
            (State::Broadcasting { .. }, Event::GetHeader { res_tx, .. }) => {
                let _ = res_tx.send(Err(ProposerApiError::DeliveringPayload));
            }

            // duplicate get_payload, proposer equivocating?
            (
                State::Broadcasting { slot_ctx, block_hash },
                Event::GetPayload { blinded, block_hash: new_block_hash, res_tx, .. },
            ) => {
                if slot_ctx.bid_slot == blinded.slot() || *block_hash == new_block_hash {
                    let _ = res_tx.send(Err(ProposerApiError::DeliveringPayload));
                } else {
                    warn!(
                        have =% block_hash,
                        received =% new_block_hash,
                        "received multiple get_payload requests"
                    );
                    let _ = res_tx.send(Err(ProposerApiError::GetPayloadAlreadyReceived));
                }
            }

            // gossip payload
            (State::Broadcasting { block_hash, slot_ctx }, Event::GossipPayload(payload)) => {
                if *block_hash == payload.execution_payload.execution_payload.block_hash &&
                    slot_ctx.bid_slot == payload.slot &&
                    slot_ctx.proposer_pubkey() == &payload.proposer_pub_key
                {
                    debug!("already broadcasting gossip payload");
                } else {
                    // is the proposer equivocating across regions?
                    warn!(
                        have.block_hash =% block_hash,
                        have.slot =% slot_ctx.bid_slot,
                        have.pubkey =%  slot_ctx.proposer_pubkey(),
                        got.block_hash =% payload.execution_payload.execution_payload.block_hash,
                        got.slot = payload.slot,
                        got.pubkey =% &payload.proposer_pub_key,
                        "mismatch in broadcasting / gossip payload")
                }
            }

            // submission unregistered
            (State::Slot { .. }, Event::Submission { res_tx, .. }) => {
                let _ = res_tx.send(Err(BuilderApiError::ProposerDutyNotFound));
            }

            // get_header unregistered
            (State::Slot { .. }, Event::GetHeader { res_tx, .. }) => {
                let _ = res_tx.send(Err(ProposerApiError::ProposerNotRegistered));
            }

            // get_payload unregistered
            (State::Slot { .. }, Event::GetPayload { res_tx, .. }) => {
                let _ = res_tx.send(Err(ProposerApiError::ProposerNotRegistered));
            }

            // gossip payload unregistered
            (State::Slot { slot }, Event::GossipPayload(payload)) => {
                warn!(curr =% slot, gossip_slot = payload.slot, "received late gossip payload");
            }
        }
    }

    fn tick(&mut self) {
        // DO we actually need a tick, could check on each gossip / submission instead
        // TODO: check pending payloads
    }
}

// do we need coordinates here
pub struct SlotContext {
    /// Head slot + 1, builders are bidding to build this slot
    bid_slot: Slot,
    /// Data about the validator registration
    registration_data: BuilderGetValidatorsResponseEntry,
    /// Payload attributes for the incoming blocks
    payload_attributes: PayloadAttributesUpdate,
    /// Current fork
    current_fork: ForkName,
    il: Option<InclusionListWithKey>,
}

impl SlotContext {
    fn validate(&self) {
        assert_eq!(self.bid_slot, self.registration_data.slot);
        assert_eq!(self.bid_slot, self.payload_attributes.slot);
        if let Some(il) = self.il.as_ref() {
            assert_eq!(self.bid_slot, il.key.0)
        }
    }
}

// cpu "intensive" stuff is:
// parsing headers, decode, hydration, sig verify, withdrawals and tx root, mergeable orders,
// get payload stuff, re-signing get header, encoding requests to JSON
