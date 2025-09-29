mod bid_sorter;
mod context;
mod decoder;
mod get_header;
mod get_payload;
mod handle;
mod merging;
mod simulator;
mod submit_block;
mod types;
mod validation;
mod worker;

use std::{cmp::Ordering, sync::Arc};

use alloy_primitives::B256;
pub use handle::AuctioneerHandle;
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    chain_info::ChainInfo,
    local_cache::LocalCache,
    RelayConfig,
};
use helix_housekeeper::{chain_event_updater::SlotData as HkSlotData, PayloadAttributesUpdate};
use helix_types::Slot;
pub use simulator::*;
use tokio::runtime;
use tracing::{debug, info, warn};
pub use types::GetPayloadResultData;

use crate::{
    auctioneer::{
        bid_sorter::BidSorter,
        context::Context,
        manager::SimulatorManager,
        types::{Event, PendingPayload, SlotData},
        worker::Worker,
    },
    builder::error::BuilderApiError,
    proposer::{MergingPoolMessage, ProposerApiError},
    Api,
};

pub fn spawn_auctioneer<A: Api>(
    chain_info: ChainInfo,
    config: RelayConfig,
    runtime: runtime::Handle,
    db: Arc<A::DatabaseService>,
    merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
    cache: LocalCache,
    top_bid_tx: tokio::sync::broadcast::Sender<bytes::Bytes>,
    slot_data_rx: crossbeam_channel::Receiver<HkSlotData>,
) -> AuctioneerHandle {
    let (worker_tx, worker_rx) = crossbeam_channel::bounded(10_000);
    let (event_tx, event_rx) = crossbeam_channel::bounded(10_000);

    assert!(config.worker_threads > 0, "need at least 1 worker thread");

    for id in 0..config.worker_threads {
        // TODO: affinity
        let worker = Worker {
            rx: worker_rx.clone(),
            tx: event_tx.clone(),
            merge_pool_tx: merge_pool_tx.clone(),
            cache: cache.clone(),
            chain_info: chain_info.clone(),
            config: config.clone(),
        };
        std::thread::Builder::new()
            .name(format!("worker-{id}"))
            .spawn(move || {
                info!(id, "starting worker thread");
                worker.run()
            })
            .unwrap();
    }

    let bid_sorter = BidSorter::new(top_bid_tx);
    let sim_manager = SimulatorManager::new(config.simulators.clone(), event_tx.clone(), runtime);
    let ctx = Context::new(chain_info, config, sim_manager, db, bid_sorter, cache);
    let auctioneer = Auctioneer::<A> { ctx, state: State::default() };

    std::thread::Builder::new()
        .name("auctioneer".to_string())
        .spawn(move || {
            info!("starting auctioneer");
            auctioneer.run(event_rx, slot_data_rx)
        })
        .unwrap();

    AuctioneerHandle::new(worker_tx, event_tx)
}

struct Auctioneer<A: Api> {
    ctx: Context<A>,
    state: State,
}

impl<A: Api> Auctioneer<A> {
    fn run(
        mut self,
        rx: crossbeam_channel::Receiver<Event>,
        slot_data_rx: crossbeam_channel::Receiver<HkSlotData>,
    ) {
        loop {
            for evt in rx.try_iter() {
                self.state.step(evt, &mut self.ctx);
            }

            if let Ok(slot_data) = slot_data_rx.try_recv() {
                let HkSlotData { bid_slot, registration_data, payload_attributes, il } = slot_data;
                let evt = Event::SlotData { bid_slot, registration_data, payload_attributes, il };
                self.state.step(evt, &mut self.ctx);
            }
        }
    }
}

enum State {
    /// Two cases:
    /// - Next proposer is not registered
    /// - Waiting for housekeeper to send all slot data to start sorting
    Slot {
        bid_slot: Slot,
        registration_data: Option<BuilderGetValidatorsResponseEntry>,
        payload_attributes: Option<PayloadAttributesUpdate>,
        il: Option<InclusionListWithMetadata>,
    },

    /// Next proposer is registered and we are processing builder bids / serving headers
    Sorting(SlotData),

    /// Received get_payload, broadcasting block
    Broadcasting { slot_data: SlotData, block_hash: B256 },
}

impl Default for State {
    fn default() -> Self {
        Self::Slot {
            bid_slot: Slot::new(0),
            registration_data: None,
            payload_attributes: None,
            il: None,
        }
    }
}

impl State {
    fn step<A: Api>(&mut self, event: Event, ctx: &mut Context<A>) {
        match (&self, event) {
            ///////////// LIFECYCLE EVENTS (ALWAYS VALID) /////////////

            // new slot data
            (
                State::Slot {
                    bid_slot: curr_slot,
                    registration_data: curr_reg,
                    payload_attributes: curr_att,
                    il: curr_il,
                },
                Event::SlotData { bid_slot, registration_data, payload_attributes, il },
            ) => {
                let (reg, att, il) = match bid_slot.cmp(curr_slot) {
                    Ordering::Less => return,
                    Ordering::Equal => {
                        // more data for current slot, maybe we can start sorting
                        // assume we can't receive different data for the same slot
                        let registration_data = curr_reg.clone().or(registration_data);
                        let payload_attributes = curr_att.clone().or(payload_attributes);
                        let il = curr_il.clone().or(il);

                        (registration_data, payload_attributes, il)
                    }
                    Ordering::Greater => (registration_data, payload_attributes, il),
                };

                *self = Self::process_slot_data(bid_slot, reg, att, il, ctx);
            }

            // new slot, either IL or slot was delivered by another relay
            (
                State::Sorting(slot_data),
                Event::SlotData { bid_slot, registration_data, payload_attributes, il },
            ) => match bid_slot.cmp(&slot_data.bid_slot) {
                Ordering::Less => (),
                Ordering::Equal => {
                    // add inclusion list
                    if slot_data.il.is_none() && il.is_some() {
                        // received new IL
                        // ugly clone but should be relatively rare
                        let slot_data = SlotData { il, ..slot_data.clone() };
                        *self = State::Sorting(slot_data);
                    }
                }
                Ordering::Greater => {
                    // another relay delivered the payload
                    *self = Self::process_slot_data(
                        bid_slot,
                        registration_data,
                        payload_attributes,
                        il,
                        ctx,
                    );
                }
            },

            // new slot, maybe delivered by us
            (
                State::Broadcasting { slot_data, block_hash },
                Event::SlotData { bid_slot, registration_data, payload_attributes, il },
            ) => match bid_slot.cmp(&slot_data.bid_slot) {
                Ordering::Less | Ordering::Equal => (),
                Ordering::Greater => {
                    if let Some(attributes) = &payload_attributes {
                        if &attributes.parent_hash != block_hash {
                            warn!(maybe_missed_slot =% slot_data.bid_slot, parent_hash =% attributes.parent_hash, broadcasting_hash =% block_hash, "new slot while broacasting different block, was the slot missed?");
                        }

                        *self = Self::process_slot_data(
                            bid_slot,
                            registration_data,
                            payload_attributes,
                            il,
                            ctx,
                        );
                    }
                }
            },

            // simulator sync status
            (_, Event::SimulatorSync { id, is_synced }) => {
                ctx.sim_manager.handle_sync_status(id, is_synced);
            }

            // late sim result
            (State::Broadcasting { .. } | State::Slot { .. }, Event::SimResult(result)) => {
                ctx.handle_simulation_result(result);
            }

            ///////////// VALID STATES / EVENTS /////////////

            // submission
            (
                State::Sorting(slot_data),
                Event::Submission {
                    submission,
                    merging_preferences,
                    withdrawals_root,
                    sequence,
                    trace,
                    res_tx,
                },
            ) => {
                ctx.handle_submission(
                    submission,
                    merging_preferences,
                    withdrawals_root,
                    sequence,
                    trace,
                    res_tx,
                    slot_data,
                );

                if let Some(state) = Self::maybe_start_broacasting(ctx, slot_data) {
                    *self = state;
                }
            }

            // get_header
            (State::Sorting(_), Event::GetHeader { params, res_tx }) => {
                ctx.handle_get_header(params, res_tx)
            }

            // get_payload
            (
                State::Sorting(slot_data),
                Event::GetPayload { blinded, block_hash, trace, res_tx },
            ) => {
                if let Some(local) = ctx.payloads.get(&block_hash) {
                    if let Some(block_hash) =
                        ctx.handle_get_payload(local.clone(), blinded, trace, res_tx, slot_data)
                    {
                        info!(bid_slot =% slot_data.bid_slot, %block_hash, "broadcasting block");
                        *self = State::Broadcasting { slot_data: slot_data.clone(), block_hash }
                    }
                } else {
                    // we may still receive the payload from builder / gossip later
                    ctx.pending_payload =
                        Some(PendingPayload { block_hash, blinded, trace, res_tx });
                }
            }

            // sim result
            (State::Sorting(_), Event::SimResult(result)) => {
                ctx.sort_simulation_result(&result);
                ctx.handle_simulation_result(result);
            }

            // gossiped payload
            (State::Sorting(slot_data), Event::GossipPayload(payload)) => {
                ctx.handle_gossip_payload(payload, slot_data);
                if let Some(state) = Self::maybe_start_broacasting(ctx, slot_data) {
                    *self = state;
                }
            }

            // merging request
            (State::Sorting(slot_data), Event::GetBestPayloadForMerging { bid_slot, res_tx }) => {
                let maybe_payload = if slot_data.bid_slot == bid_slot {
                    ctx.get_best_mergeable_payload()
                } else {
                    None
                };

                let _ = res_tx.send(maybe_payload);
            }

            ///////////// INVALID STATES / EVENTS /////////////

            // late submission
            (
                State::Broadcasting { slot_data: slot_ctx, .. },
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
                State::Broadcasting { slot_data: slot_ctx, block_hash },
                Event::GetPayload { blinded, block_hash: new_block_hash, res_tx, .. },
            ) => {
                if slot_ctx.bid_slot == blinded.slot() && *block_hash == new_block_hash {
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

            // gossiped payload, proposer equivocating?
            (
                State::Broadcasting { block_hash, slot_data: slot_ctx },
                Event::GossipPayload(payload),
            ) => {
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
            (State::Slot { bid_slot, .. }, Event::GossipPayload(payload)) => {
                warn!(curr =% bid_slot, gossip_slot = payload.slot, "received early or late gossip payload");
            }

            // merging
            (
                State::Slot { .. } | State::Broadcasting { .. },
                Event::GetBestPayloadForMerging { res_tx, .. },
            ) => {
                let _ = res_tx.send(None);
            }
        }
    }

    fn process_slot_data<A: Api>(
        bid_slot: Slot,
        registration_data: Option<BuilderGetValidatorsResponseEntry>,
        payload_attributes: Option<PayloadAttributesUpdate>,
        il: Option<InclusionListWithMetadata>,
        ctx: &mut Context<A>,
    ) -> Self {
        match (registration_data, payload_attributes) {
            (Some(registration_data), Some(payload_attributes)) => {
                let current_fork = ctx.chain_info.fork_at_slot(bid_slot);

                let slot_data =
                    SlotData { bid_slot, registration_data, payload_attributes, current_fork, il };

                info!(%bid_slot, "received all slot data, start sorting");
                ctx.on_new_slot(bid_slot);
                State::Sorting(slot_data)
            }

            (Some(registration_data), None) => State::Slot {
                bid_slot,
                registration_data: Some(registration_data),
                payload_attributes: None,
                il,
            },

            (None, Some(payload_attributes)) => State::Slot {
                bid_slot,
                registration_data: None,
                payload_attributes: Some(payload_attributes),
                il,
            },

            (None, None) => {
                State::Slot { bid_slot, registration_data: None, payload_attributes: None, il }
            }
        }
    }

    /// Note that we may still fail to actually broacast the block after we change State, eg. if the
    /// request came to late, or if we fail to broadcast the block
    fn maybe_start_broacasting<A: Api>(ctx: &mut Context<A>, slot_data: &SlotData) -> Option<Self> {
        let block_hash = ctx.maybe_try_unblind(slot_data)?;
        info!(bid_slot =% slot_data.bid_slot, %block_hash, "broadcasting block");
        Some(State::Broadcasting { slot_data: slot_data.clone(), block_hash })
    }
}
