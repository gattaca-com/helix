#![allow(unused)]

mod bid_sorter;
mod sim_manager;
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
    metrics::SimulatorMetrics,
    simulator::BlockSimError,
    utils::utcnow_ns,
    BuilderInfo, GetPayloadTrace, RelayConfig, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{
    BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ForkName, GetPayloadResponse,
    HydrationCache, SignedBidSubmission, SignedBlindedBeaconBlock, SignedBuilderBid, Slot,
    VersionedSignedProposal,
};
use moka::ops::compute::Op;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::{
    builder::{
        error::BuilderApiError,
        simulator_2::{
            bid_sorter::BidSorter,
            sim_manager::{SimulationResult, SimulatorManager},
            worker::{GetPayloadResult, GetPayloadResultData, Submission, SubmissionResult},
        },
        BlockSimRequest, SimReponse, SimulatorRequest,
    },
    gossiper::{grpc_gossiper::GrpcGossiperClientManager, types::BroadcastGetPayloadParams},
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
            let Ok(evt) = rx.try_recv() else {
                continue;
            };

            self.state.step(evt, &mut self.ctx);
        }
    }
}

struct Context<A: Api> {
    chain_info: ChainInfo,
    builder_info: FxHashMap<BlsPublicKeyBytes, BuilderInfo>,
    unknown_builder_info: BuilderInfo,
    gossiper: GrpcGossiperClientManager,
    simulator_manager: SimulatorManager,
    // on transition to new slot, send ProposerApiError::NoExecutionPayloadFound for these
    // TODO: check for which hash they sent the request to avoid duplicates
    pending_payloads: Vec<PendingPayload>,

    // flags
    // failsafe_triggered + accept_optimistic
    can_process_optimistic: bool,

    db: Arc<A::DatabaseService>,
    config: RelayConfig,
}

struct PendingPayload {
    blinded: SignedBlindedBeaconBlock,
    res_tx: oneshot::Sender<GetPayloadResult>,
    retry_at: Instant,
}

impl<A: Api> Context<A> {
    fn clear(&mut self) {}
}

struct SortingData {
    slot: SlotContext,
    sort: BidSorter,
    seen_block_hashes: FxHashSet<B256>,
    sequence: FxHashMap<BlsPublicKeyBytes, u64>,
    hydration_cache: HydrationCache,
    payloads: FxHashMap<B256, SignedBidSubmission>,
    inclusion_list: Option<InclusionListWithMetadata>,
}

enum State {
    /// Next proposer is registered and we are processing builder bids
    Sorting(SortingData),
    /// Processing get_payload, broadcasting block
    Broadcasting { slot_ctx: SlotContext, block_hash: B256 },
    /// Next proposer is not registered, reject bids
    Slot { slot: Slot },
}

pub enum Event {
    Submission {
        submission: Submission,
        sequence: Option<u64>,
        trace: SubmissionTrace,
        res_tx: oneshot::Sender<SubmissionResult>,
    },
    /// Assume already some validation (so we don't have to wait here)
    /// timing games already done
    GetHeader {
        params: GetHeaderParams,
        res_tx: oneshot::Sender<Result<BuilderBid, ProposerApiError>>,
    },
    // Receive multiple of these potentially, assume some validation
    GetPayload {
        block_hash: B256,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
    },
    NewSlot,
    SimResult(SimulationResult),
    MergeResult,
    SimulatorSync {
        id: usize,
        is_synced: bool,
    },
}

impl State {
    fn step<A: Api>(&mut self, event: Event, ctx: &mut Context<A>) {
        match (self, event) {
            (
                State::Sorting(sorting),
                Event::Submission { submission, sequence, mut trace, res_tx },
            ) => match sorting.handle_block_submission(submission, sequence, &mut trace, ctx) {
                Ok((submission, optimistic_version)) => {
                    let res_tx = if optimistic_version.is_optimistic() {
                        let _ = res_tx.send(Ok(()));
                        None
                    } else {
                        Some(res_tx)
                    };

                    sorting.handle_2(submission, trace, optimistic_version, ctx, res_tx);
                }

                Err(err) => {
                    let _ = res_tx.send(Err(err));
                }
            },
            (State::Sorting(sorting), Event::GetHeader { params, res_tx }) => {}

            (State::Sorting(sorting), Event::GetPayload { blinded, block_hash, trace, res_tx }) => {
                // TODO: this serializes the proto, and spawns some tasks so needs a runtime
                // ctx.gossiper.broadcast_get_payload(BroadcastGetPayloadParams {
                //     signed_blinded_beacon_block: blinded.clone(),
                //     request_id: Default::default(),
                // });

                if let Some(payload) = sorting.payloads.get(&block_hash) {
                    let res = sorting.handle_get_payload(blinded, payload, trace).map(
                        |(to_proposer, to_publish, trace)| {
                            let proposer_pubkey =
                                sorting.slot.registration_data.entry.registration.message.pubkey;

                            GetPayloadResultData {
                                to_proposer,
                                to_publish,
                                trace,
                                proposer_pubkey,
                                fork: sorting.slot.current_fork,
                            }
                        },
                    );

                    let _ = res_tx.send(res);
                } else {
                    // we may still receive the payload from builder / gossip, save request for
                    // later
                    ctx.pending_payloads.push(PendingPayload {
                        blinded,
                        res_tx,
                        retry_at: Instant::now() + Duration::from_millis(20),
                    });
                }
            }

            (State::Sorting(sorting), Event::SimResult(resp)) => {
                sorting.handle_sim_result(resp, ctx)
            }

            (State::Sorting(sorting), Event::SimulatorSync { id, is_synced }) => {
                ctx.simulator_manager.handle_sync_status(id, is_synced);
            }

            (_, Event::NewSlot) => {
                ctx.clear();
            }

            (
                State::Broadcasting { slot_ctx, .. },
                Event::Submission { submission, res_tx, .. },
            ) => {
                let _ = res_tx.send(Err(BuilderApiError::DeliveringPayload {
                    bid_slot: submission.bid_slot(),
                    delivering: slot_ctx.bid_slot.as_u64(),
                }));
            }

            (State::Slot { .. } | State::Broadcasting { .. }, Event::GetHeader { res_tx, .. }) => {
                let _ = res_tx.send(Err(ProposerApiError::ProposerNotRegistered));
            }

            (State::Sorting(sorting), Event::GetHeader { params, res_tx }) => {
                let res = sorting.handle_get_header(params);
                let _ = res_tx.send(res);
            }

            (State::Broadcasting { slot_ctx, block_hash }, Event::GetPayload { res_tx, .. }) => {
                // return Err(AuctioneerError::PastSlotAlreadyDelivered);
                // return Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot);
            }

            _ => todo!(),
        }
    }
}

impl SortingData {
    fn handle_get_header(&self, params: GetHeaderParams) -> Result<BuilderBid, ProposerApiError> {
        if params.slot != self.slot.bid_slot.as_u64() {
            return Err(ProposerApiError::RequestWrongSlot {
                request_slot: params.slot,
                bid_slot: self.slot.bid_slot.as_u64(),
            });
        }

        let Some(bid) = self.sort.get_header() else {
            return Err(ProposerApiError::NoBidPrepared);
        };

        // TODO: this check is proably useless
        if bid.value == U256::ZERO {
            return Err(ProposerApiError::BidValueZero);
        }

        Ok(bid)
    }

    fn handle_get_payload(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: &SignedBidSubmission,
        trace: GetPayloadTrace,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal, GetPayloadTrace), ProposerApiError>
    {
        // TODO: use trace
        let (to_proposer, to_publish) = self.validate_and_unblind(blinded, local)?;
        Ok((to_proposer, to_publish, trace))
    }

    fn handle_sim_result<A: Api>(&mut self, result: SimulationResult, ctx: &mut Context<A>) {
        ctx.simulator_manager.handle_task_response(result.id, result.paused_until);

        let builder = result.builder_pubkey;
        let block_hash = result.block_hash;

        if let Err(err) = result.result.as_ref() {
            if let Some(builder_info) = ctx.builder_info.get_mut(&builder) {
                if builder_info.is_optimistic {
                    if err.is_already_known() {
                        warn!(
                            %builder,
                            %block_hash,
                            "Block already known. Skipping demotion"
                        );
                    } else if err.is_too_old() {
                        warn!(
                            %builder,
                            %block_hash,
                            "Block is too old. Skipping demotion"
                        );
                    } else if err.is_temporary() {
                        warn!(
                            %builder,
                            %block_hash,
                            "Error is temporary. Skipping demotion"
                        );
                    } else {
                        warn!(
                            %builder,
                            %block_hash,
                            %err,
                            "Block simulation resulted in an error. Demoting builder...",
                        );

                        SimulatorMetrics::demotion_count();
                        self.sort.demote(builder);

                        builder_info.is_optimistic = false;
                        builder_info.is_optimistic_for_regional_filtering = false;

                        let db = ctx.db.clone();
                        let reason = err.to_string();
                        let bid_slot = self.slot.bid_slot;

                        tokio::spawn(async move {
                            if let Err(err) = db
                                .db_demote_builder(bid_slot.into(), &builder, &block_hash, reason)
                                .await
                            {
                                // TODO:
                                // self.failsafe_triggered.store(true, Ordering::Relaxed);
                                error!(%builder, %err, "failed to demote builder in database"
                                );
                            }
                        });
                    }
                }
            };
        }

        if let Some(res_tx) = result.res_tx {
            let submission_result = result.result.map_err(BuilderApiError::from);
            let _ = res_tx.send(submission_result);
        }
    }

    fn handle_block_submission<A: Api>(
        &mut self,
        submission: Submission,
        sequence: Option<u64>,
        trace: &mut SubmissionTrace,
        ctx: &Context<A>,
    ) -> Result<(SignedBidSubmission, OptimisticVersion), BuilderApiError> {
        let submission = match submission {
            Submission::Full(full) => full,
            Submission::Dehydrated(dehydrated) => {
                // TODO: save metrics
                let (payload, _, _) = dehydrated.hydrate(&mut self.hydration_cache)?;
                // let result = match dehydrated_bid_submission.hydrate(&mut cache) {
                //     Ok((result, tx_cache_hits, blob_cache_hits)) => {
                //         HYDRATION_CACHE_HITS
                //             .with_label_values(&["transaction"])
                //             .inc_by(tx_cache_hits as u64);
                //         HYDRATION_CACHE_HITS
                //             .with_label_values(&["blob"])
                //             .inc_by(blob_cache_hits as u64);

                //         Ok(result)
                //     }
                //     Err(err) => Err(err),
                // };

                payload.validate_payload_ssz_lengths()?;

                payload
            }
        };

        let builder_info = ctx
            .builder_info
            .get(submission.builder_public_key())
            .unwrap_or(&ctx.unknown_builder_info);

        self.validate_submission(&submission, sequence, builder_info)?;

        let mut optimistic_version = OptimisticVersion::NotOptimistic;
        if ctx.can_process_optimistic &&
            self.should_process_optimistically(&submission, builder_info)
        {
            optimistic_version = OptimisticVersion::V1;
            // self.sort.sort(&submission);
        }

        // block merging
        // TODO: sort also after simulation!

        Ok((submission, optimistic_version))
    }

    fn handle_2<A: Api>(
        &mut self,
        submission: SignedBidSubmission,
        submission_trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        ctx: &mut Context<A>,
        res_tx: Option<oneshot::Sender<SubmissionResult>>,
    ) {
        let is_top_bid = self.sort.is_top_bid(&submission);
        let inclusion_list = self.inclusion_list.clone();

        let request = BlockSimRequest::new(
            self.slot.registration_data.entry.registration.message.gas_limit,
            &submission,
            self.slot.registration_data.entry.preferences.clone(),
            self.slot.payload_attributes.payload_attributes.parent_beacon_block_root,
            inclusion_list,
        );

        // send to sim_manager queue, use another oneshot or simply the sim results?
        let req = SimulatorRequest {
            request,
            on_receive_ns: submission_trace.receive,
            is_top_bid,
            is_optimistic: optimistic_version.is_optimistic(),
            res_tx,
        };
        ctx.simulator_manager.handle_sim_request(req);

        self.payloads.insert(*submission.block_hash(), submission.clone());

        let db = ctx.db.clone();
        tokio::spawn(async move {
            if let Err(err) =
                db.store_block_submission(submission, submission_trace, optimistic_version).await
            {
                error!(%err, "failed to store block submission")
            }
        });
    }

    fn should_process_optimistically(
        &self,
        submission: &SignedBidSubmission,
        builder_info: &BuilderInfo,
    ) -> bool {
        if builder_info.is_optimistic && submission.message().value <= builder_info.collateral {
            if self.slot.registration_data.entry.preferences.filtering.is_regional() &&
                !builder_info.can_process_regional_slot_optimistically()
            {
                return false;
            }

            return true;
        }

        false
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

// cpu intensive stuff is:
// parsing headers, decode, hydration, sig verify, withdrawals and tx root, mergeable orders,
// get payload stuff, re-signing get header, encoding requests to JSON
