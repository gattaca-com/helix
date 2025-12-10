mod bid_adjustor;
mod bid_sorter;
mod block_merger;
mod context;
mod decoder;
mod get_header;
mod get_payload;
mod handle;
mod simulator;
mod submit_block;
mod types;
mod validation;
mod worker;

use std::{
    cmp::Ordering,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
pub use block_merger::OrderValidationError;
pub use handle::{AuctioneerHandle, RegWorkerHandle};
use helix_common::{
    RelayConfig,
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::{STATE_TRANSITION_COUNT, STATE_TRANSITION_LATENCY, WORKER_QUEUE_LEN, WORKER_UTIL},
    record_submission_step,
    utils::pin_thread_to_core,
};
use helix_types::Slot;
use rustc_hash::FxHashMap;
pub use simulator::*;
use tracing::{debug, error, info, info_span, trace, warn};
pub use types::{Event, GetPayloadResultData, PayloadBidData, PayloadEntry};
use worker::{RegWorker, SubWorker};

pub use crate::auctioneer::bid_adjustor::{BidAdjustor, DefaultBidAdjustor};
use crate::{
    PostgresDatabaseService,
    api::{builder::error::BuilderApiError, proposer::ProposerApiError},
    auctioneer::{
        bid_sorter::BidSorter,
        context::Context,
        manager::SimulatorManager,
        types::{PendingPayload, SlotData},
    },
    housekeeper::PayloadAttributesUpdate,
};

// TODO: tidy up builder and proposer api state, and spawn in a separate function
pub fn spawn_workers<B: BidAdjustor>(
    chain_info: ChainInfo,
    config: RelayConfig,
    db: Arc<PostgresDatabaseService>,
    cache: LocalCache,
    bid_adjustor: B,
    top_bid_tx: tokio::sync::broadcast::Sender<bytes::Bytes>,
    event_channel: (crossbeam_channel::Sender<Event>, crossbeam_channel::Receiver<Event>),
) -> (AuctioneerHandle, RegWorkerHandle) {
    let (sub_worker_tx, sub_worker_rx) = crossbeam_channel::bounded(10_000);
    let (reg_worker_tx, reg_worker_rx) = crossbeam_channel::bounded(100_000);
    let (event_tx, event_rx) = event_channel;

    if config.is_registration_instance {
        for core in config.cores.reg_workers.clone() {
            let worker = RegWorker::new(core, chain_info.clone());
            let rx = reg_worker_rx.clone();

            std::thread::Builder::new()
                .name(format!("worker-{core}"))
                .spawn(move || {
                    pin_thread_to_core(core);
                    worker.run(rx)
                })
                .unwrap();
        }
    }

    if config.is_submission_instance {
        for core in config.cores.sub_workers.clone() {
            let worker = SubWorker::new(core, event_tx.clone(), cache.clone(), chain_info.clone());
            let rx = sub_worker_rx.clone();

            std::thread::Builder::new()
                .name(format!("worker-{core}"))
                .spawn(move || {
                    pin_thread_to_core(core);
                    worker.run(rx)
                })
                .unwrap();
        }

        let auctioneer_core = config.cores.auctioneer;

        let bid_sorter = BidSorter::new(top_bid_tx);
        let sim_manager = SimulatorManager::new(config.simulators.clone(), event_tx.clone());
        let ctx =
            Context::new(chain_info, config, sim_manager, db, bid_sorter, cache, bid_adjustor);
        let id = format!("auctioneer_{auctioneer_core}");
        let auctioneer = Auctioneer { ctx, state: State::default(), tel: Telemetry::new(id) };

        std::thread::Builder::new()
            .name("auctioneer".to_string())
            .spawn(move || {
                pin_thread_to_core(auctioneer_core);
                auctioneer.run(event_rx)
            })
            .unwrap();
    }

    (AuctioneerHandle::new(sub_worker_tx, event_tx), RegWorkerHandle::new(reg_worker_tx))
}

struct Auctioneer<B: BidAdjustor> {
    ctx: Context<B>,
    state: State,
    tel: Telemetry,
}

impl<B: BidAdjustor> Auctioneer<B> {
    fn run(mut self, rx: crossbeam_channel::Receiver<Event>) {
        let _span = info_span!("auctioneer").entered();
        info!("starting");

        loop {
            for evt in rx.try_iter() {
                self.state.step(evt, &mut self.ctx, &mut self.tel);
            }

            self.tel.telemetry(&rx);
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
        payload_attributes_map: FxHashMap<B256, PayloadAttributesUpdate>,
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
            payload_attributes_map: Default::default(),
            il: None,
        }
    }
}

// TODO: tokio metrics

impl State {
    fn step<B: BidAdjustor>(&mut self, event: Event, ctx: &mut Context<B>, tel: &mut Telemetry) {
        let start = Instant::now();
        let start_state = self.as_str();
        let event_tag = event.as_str();

        self._step(event, ctx);

        let end_state = self.as_str();
        let step_dur = start.elapsed();
        tel.loop_worked += step_dur;

        let task_name = format!("{start_state}+{event_tag}->{end_state}");

        STATE_TRANSITION_COUNT.with_label_values(&[&task_name]).inc();
        STATE_TRANSITION_LATENCY
            .with_label_values(&[&task_name])
            .observe(step_dur.as_nanos() as f64 / 1000.);
    }

    fn _step<B: BidAdjustor>(&mut self, event: Event, ctx: &mut Context<B>) {
        match (&self, event) {
            ///////////// LIFECYCLE EVENTS (ALWAYS VALID) /////////////

            // new slot data
            (
                State::Slot {
                    bid_slot: curr_slot,
                    registration_data: curr_reg,
                    payload_attributes_map,
                    il: curr_il,
                },
                Event::SlotData { bid_slot, registration_data, payload_attributes, il },
            ) => {
                let (reg, att_map, il) = match bid_slot.cmp(curr_slot) {
                    Ordering::Less => return,
                    Ordering::Equal => {
                        // more data for current slot, maybe we can start sorting
                        // assume we can't receive different data for the same slot
                        let registration_data = curr_reg.clone().or(registration_data);
                        let il = curr_il.clone().or(il);

                        (registration_data, payload_attributes_map.clone(), il)
                    }
                    Ordering::Greater => {
                        if *curr_slot > 0 {
                            assert_eq!(
                                bid_slot,
                                *curr_slot + 1,
                                "gap in slot data received (slot)"
                            );
                        }

                        ctx.on_new_slot(bid_slot);
                        (registration_data, FxHashMap::default(), il)
                    }
                };

                *self =
                    Self::process_slot_data(bid_slot, att_map, reg, payload_attributes, il, ctx);
            }

            // new slot, either IL or slot was delivered by another relay
            (
                State::Sorting(slot_data),
                Event::SlotData { bid_slot, registration_data, payload_attributes, il },
            ) => match bid_slot.cmp(&slot_data.bid_slot) {
                Ordering::Less => (),
                Ordering::Equal => {
                    // check fork
                    if let Some(update) = payload_attributes &&
                        !slot_data.payload_attributes_map.contains_key(&update.parent_hash)
                    {
                        info!(bid_slot =% slot_data.bid_slot, received =? update.parent_hash, sorting =? slot_data.payload_attributes_map.keys(), "sorting for an additional fork");

                        // ugly clone but should be relatively rare
                        let mut slot_data = slot_data.clone();
                        slot_data.payload_attributes_map.insert(update.parent_hash, update);

                        *self = State::Sorting(slot_data);
                    } else if slot_data.il.is_none() && il.is_some() {
                        info!(
                            txs = il.as_ref().map(|i| i.txs.len()).unwrap_or_default(),
                            "received new inclusion list"
                        );

                        // ugly clone but should be relatively rare
                        let slot_data = SlotData { il, ..slot_data.clone() };
                        *self = State::Sorting(slot_data);
                    }
                }
                Ordering::Greater => {
                    assert_eq!(
                        bid_slot,
                        slot_data.bid_slot + 1,
                        "gap in slot data received (sort)"
                    );

                    ctx.on_new_slot(bid_slot);
                    // another relay delivered the payload
                    *self = Self::process_slot_data(
                        bid_slot,
                        FxHashMap::default(),
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
                    assert_eq!(
                        bid_slot,
                        slot_data.bid_slot + 1,
                        "gap in slot data received (broadcast)"
                    );

                    if let Some(attributes) = &payload_attributes &&
                        &attributes.parent_hash != block_hash
                    {
                        warn!(
                            maybe_missed_slot =% slot_data.bid_slot,
                            parent_hash =% attributes.parent_hash,
                            broadcasting_hash =% block_hash,
                            "new slot while broacasting different block, was the slot missed?");
                    }

                    ctx.on_new_slot(bid_slot);
                    *self = Self::process_slot_data(
                        bid_slot,
                        FxHashMap::default(),
                        registration_data,
                        payload_attributes,
                        il,
                        ctx,
                    );
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

            // late merge result
            (State::Broadcasting { .. } | State::Slot { .. }, Event::MergeResult((id, _))) => {
                ctx.handle_simulation_result((id, None));
            }

            ///////////// VALID STATES / EVENTS /////////////

            // submission
            (
                State::Sorting(slot_data),
                Event::Submission { submission_data, res_tx, span, sent_at },
            ) => {
                record_submission_step("loop_recv", sent_at.elapsed());

                let _guard = span.enter();
                trace!("received in auctioneer");

                ctx.handle_submission(submission_data, res_tx, slot_data);

                trace!("finished processing");
                drop(_guard);

                if let Some(state) = Self::maybe_start_broacasting(ctx, slot_data) {
                    *self = state;
                }
            }

            // get_header
            (State::Sorting(slot_data), Event::GetHeader { params, res_tx, span }) => {
                let _guard = span.enter();
                trace!("received in auctioneer");

                if slot_data.bid_slot != params.slot {
                    let _ = res_tx.send(Err(ProposerApiError::RequestWrongSlot {
                        request_slot: params.slot,
                        bid_slot: slot_data.bid_slot.into(),
                    }));
                } else if !slot_data.payload_attributes_map.contains_key(&params.parent_hash) {
                    // proposer is on a different fork
                    warn!(req =% params.parent_hash, have =? slot_data.payload_attributes_map.keys(), "get header for unknown parent hash");
                    let _ = res_tx.send(Err(ProposerApiError::NoBidPrepared));
                } else if slot_data.registration_data.entry.registration.message.pubkey !=
                    params.pubkey
                {
                    warn!(req =% params.pubkey, this =% slot_data.registration_data.entry.registration.message.pubkey, "get header for mismatched proposer");
                    let _ = res_tx.send(Err(ProposerApiError::NoBidPrepared));
                } else {
                    ctx.handle_get_header(params, res_tx)
                }

                trace!("finished processing");
                drop(_guard);
            }

            // get_payload
            (
                State::Sorting(slot_data),
                Event::GetPayload { blinded, block_hash, trace, res_tx, span },
            ) => {
                let _guard = span.enter();
                trace!("received in auctioneer");

                if let Some(local) = ctx.payloads.get(&block_hash) {
                    if let Some(block_hash) = ctx.handle_get_payload(
                        local.payload_and_blobs.clone(),
                        *blinded,
                        trace,
                        res_tx,
                        slot_data,
                        local.bid_data.clone(),
                    ) {
                        info!(bid_slot =% slot_data.bid_slot, %block_hash, "broadcasting block");
                        *self = State::Broadcasting { slot_data: slot_data.clone(), block_hash }
                    }
                } else if ctx.pending_payload.is_none() {
                    // we may still receive the payload from builder / gossip later
                    info!(bid_slot =% slot_data.bid_slot, %block_hash, "received get_payload but don't have block, keep as pending");
                    ctx.pending_payload =
                        Some(PendingPayload { block_hash, blinded: *blinded, trace, res_tx });
                } else {
                    // keep only one pending per slot
                    let _ = res_tx.send(Err(ProposerApiError::GetPayloadAlreadyReceived));
                }

                trace!("finished processing");
                drop(_guard);
            }

            // sim result
            (State::Sorting(slot_data), Event::SimResult(mut result)) => {
                if result.1.as_ref().is_some_and(|r| r.submission.slot() == slot_data.bid_slot) {
                    ctx.sort_simulation_result(&mut result);
                }

                ctx.handle_simulation_result(result);
            }

            // gossiped payload
            (State::Sorting(slot_data), Event::GossipPayload(payload)) => {
                ctx.handle_gossip_payload(payload, slot_data);
                if let Some(state) = Self::maybe_start_broacasting(ctx, slot_data) {
                    *self = state;
                }
            }

            (State::Sorting(_), Event::MergeResult((id, result))) => {
                match result {
                    Ok(response) => {
                        ctx.handle_merge_response(response);
                    }
                    Err(err) => {
                        error!(%err, bid_slot =% id, "failed to merge block");
                    }
                }

                ctx.handle_simulation_result((id, None));
            }

            ///////////// INVALID STATES / EVENTS /////////////

            // late submission
            (
                State::Broadcasting { slot_data: slot_ctx, .. },
                Event::Submission { submission_data, res_tx, .. },
            ) => {
                let _ = res_tx.send(Err(BuilderApiError::DeliveringPayload {
                    bid_slot: submission_data.bid_slot(),
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
            (State::Broadcasting { block_hash, slot_data }, Event::GossipPayload(payload)) => {
                if *block_hash == payload.execution_payload.execution_payload.block_hash &&
                    slot_data.bid_slot == payload.slot &&
                    slot_data.proposer_pubkey() == &payload.proposer_pub_key
                {
                    debug!("already broadcasting gossip payload");
                } else {
                    // ignore gossiped payload, get payload has already being served
                    info!(
                        broadcast.block_hash =% block_hash,
                        gossip.block_hash =% payload.execution_payload.execution_payload.block_hash,
                        slot =% slot_data.bid_slot,
                        "ignoring gossiped payload while broadcasting")
                }
            }

            // submission unregistered
            (State::Slot { bid_slot, .. }, Event::Submission { res_tx, submission_data, .. }) => {
                if submission_data.bid_slot() == bid_slot.as_u64() {
                    // either not registered or waiting for full data from housekepper
                    let _ = res_tx.send(Err(BuilderApiError::ProposerDutyNotFound));
                } else {
                    let _ = res_tx.send(Err(BuilderApiError::BidValidation(
                        helix_types::BlockValidationError::SubmissionForWrongSlot {
                            expected: *bid_slot,
                            got: submission_data.bid_slot().into(),
                        },
                    )));
                }
            }

            // get_header not sorting
            (State::Slot { bid_slot, .. }, Event::GetHeader { res_tx, params, .. }) => {
                if params.slot == bid_slot.as_u64() {
                    // either not registered or waiting for full data from housekepper
                    let _ = res_tx.send(Err(ProposerApiError::NoBidPrepared));
                } else {
                    let _ = res_tx.send(Err(ProposerApiError::RequestWrongSlot {
                        request_slot: params.slot,
                        bid_slot: bid_slot.as_u64(),
                    }));
                }
            }

            // get_payload unregistered
            (State::Slot { bid_slot, .. }, Event::GetPayload { res_tx, blinded, .. }) => {
                if bid_slot.saturating_sub(Slot::from(1u64)) == blinded.slot() {
                    let _ = res_tx.send(Err(ProposerApiError::RequestForPastSlot {
                        head_slot: blinded.slot(),
                        request_slot: blinded.slot(),
                    }));
                } else {
                    let _ = res_tx.send(Err(ProposerApiError::ProposerNotRegistered));
                }
            }

            // gossip payload unregistered
            (State::Slot { bid_slot, .. }, Event::GossipPayload(payload)) => {
                // avoid logging if just one slot late
                if bid_slot.as_u64() != payload.slot + 1 {
                    warn!(curr =% bid_slot, gossip_slot = payload.slot, "received early or late gossip payload");
                }
            }
        }
    }

    fn process_slot_data<B: BidAdjustor>(
        bid_slot: Slot,
        mut payload_attributes_map: FxHashMap<B256, PayloadAttributesUpdate>,
        registration_data: Option<BuilderGetValidatorsResponseEntry>,
        payload_attributes_update: Option<PayloadAttributesUpdate>,
        il: Option<InclusionListWithMetadata>,
        ctx: &mut Context<B>,
    ) -> Self {
        if let Some(update) = payload_attributes_update {
            payload_attributes_map.insert(update.parent_hash, update);
        }

        match (registration_data, payload_attributes_map.is_empty()) {
            (Some(registration_data), false) => {
                let current_fork = ctx.chain_info.fork_at_slot(bid_slot);

                info!(%bid_slot, attributes = payload_attributes_map.len(), "processed slot data, start sorting");

                let slot_data = SlotData {
                    bid_slot,
                    registration_data,
                    payload_attributes_map,
                    current_fork,
                    il,
                };

                State::Sorting(slot_data)
            }

            (Some(registration_data), true) => {
                info!(%bid_slot, registration = true, attributes = 0, il = il.is_some(), "processed slot data");
                State::Slot {
                    bid_slot,
                    registration_data: Some(registration_data),
                    payload_attributes_map,
                    il,
                }
            }

            (None, _) => {
                info!(%bid_slot, registration = false, attributes = payload_attributes_map.len(), il = il.is_some(), "processed slot data");
                State::Slot { bid_slot, registration_data: None, payload_attributes_map, il }
            }
        }
    }

    /// Check whether we received the payload we were waiting for the pending get_payload, this
    /// makes sense to be called only when we add a new payload ie. when receiving a new submission
    /// or from gossip
    fn maybe_start_broacasting<B: BidAdjustor>(
        ctx: &mut Context<B>,
        slot_data: &SlotData,
    ) -> Option<Self> {
        let block_hash = ctx.maybe_try_unblind(slot_data)?;
        info!(bid_slot =% slot_data.bid_slot, %block_hash, "broadcasting block");

        // Note that we may still fail to actually broacast the block after we change State, eg. if
        // the request came to late, or if we fail to broadcast the block
        Some(State::Broadcasting { slot_data: slot_data.clone(), block_hash })
    }
}

impl State {
    fn as_str(&self) -> &'static str {
        match &self {
            State::Slot { .. } => "Slot",
            State::Sorting(_) => "Sorting",
            State::Broadcasting { .. } => "Broadcasting",
        }
    }
}

pub struct Telemetry {
    id: String,
    work: Duration,
    spin: Duration,
    next_record: Instant,
    loop_start: Instant,
    loop_worked: Duration,
}

impl Telemetry {
    const REPORT_FREQ: Duration = Duration::from_millis(500);

    fn new(id: String) -> Self {
        Self {
            id,
            work: Default::default(),
            spin: Duration::default(),
            next_record: Instant::now() + Self::REPORT_FREQ,
            loop_start: Instant::now(),
            loop_worked: Duration::ZERO,
        }
    }

    fn telemetry<T>(&mut self, rx: &crossbeam_channel::Receiver<T>) {
        let now = Instant::now();
        let loop_elapsed = now.duration_since(self.loop_start);

        if loop_elapsed.is_zero() {
            return;
        }

        let worked = std::cmp::min(self.loop_worked, loop_elapsed);
        self.work += worked;
        self.spin += loop_elapsed - worked;

        self.loop_worked -= worked;
        self.loop_start = now;

        if self.next_record < now {
            self.next_record = now + Self::REPORT_FREQ;
            let spin = std::mem::take(&mut self.spin);
            let work = std::mem::take(&mut self.work);

            let total = spin + work;
            let util = if total.is_zero() { 0.0 } else { work.as_secs_f64() / total.as_secs_f64() };

            WORKER_UTIL.with_label_values(&[&self.id]).observe(util);
            WORKER_QUEUE_LEN.with_label_values(&["auctioneer"]).observe(rx.len() as f64);
        }
    }
}
