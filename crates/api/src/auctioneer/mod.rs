mod bid_sorter;
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

use std::sync::Arc;

use alloy_primitives::B256;
pub use handle::AuctioneerHandle;
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithKey},
    chain_info::ChainInfo,
    local_cache::LocalCache,
    RelayConfig,
};
use helix_housekeeper::PayloadAttributesUpdate;
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
        types::{Event, SlotContext, SortingData},
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
) -> AuctioneerHandle {
    let (worker_tx, worker_rx) = crossbeam_channel::bounded(10_000);
    let (event_tx, event_rx) = crossbeam_channel::bounded(10_000);

    assert!(config.auctioneer.worker_threads > 0, "need at least 1 worker thread");

    for id in 0..config.auctioneer.worker_threads {
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
            auctioneer.run(event_rx)
        })
        .unwrap();

    AuctioneerHandle::new(worker_tx, event_tx)
}

struct Auctioneer<A: Api> {
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

enum State {
    /// Two cases:
    /// - Next proposer is not registered
    /// - Waiting for housekeeper to send all data to start sorting
    Slot {
        slot: Slot,
        registration_data: Option<BuilderGetValidatorsResponseEntry>,
        payload_attributes: Option<PayloadAttributesUpdate>,
        il: Option<InclusionListWithKey>,
    },

    /// Next proposer is registered and we are processing builder bids
    Sorting(SortingData),

    /// Processing get_payload, broadcasting block
    Broadcasting { slot_ctx: SlotContext, block_hash: B256 },
}

impl Default for State {
    fn default() -> Self {
        Self::Slot {
            slot: Slot::new(0),
            registration_data: None,
            payload_attributes: None,
            il: None,
        }
    }
}

impl State {
    // TODO: state transitions
    fn step<A: Api>(&mut self, event: Event, ctx: &mut Context<A>) {
        match (self, event) {
            ///////////// LIFECYCLE EVENTS (ALWAYS VALID) /////////////

            // new slot
            (_, Event::SlotData { bid_slot, registration_data, payload_attributes, il }) => {
                // if bid_slot < ctx.last_bid_slot {
                //     return;
                // }

                // if bid_slot > ctx.last_bid_slot {
                // ctx.on_new_slot(Default::default());
                // match (registration_data, payload_attributes) {
                //     (Some(registration_data), Some(payload_attributes)) => {
                //         let state = SortingData {
                //             slot: bid_slot,
                //             sort: todo!(),
                //             seen_block_hashes: todo!(),
                //             sequence: todo!(),
                //             hydration_cache: todo!(),
                //             payloads: todo!(),
                //             inclusion_list: todo!(),
                //         };
                //         *self = State::Sorting(())
                //     }
                // }
                // }
            }

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
                State::Sorting(sorting),
                Event::Submission { submission, withdrawals_root, sequence, trace, res_tx },
            ) => ctx.handle_submission(
                submission,
                withdrawals_root,
                sequence,
                trace,
                res_tx,
                &sorting.slot,
            ),

            // get_header
            (State::Sorting(sorting), Event::GetHeader { params, res_tx }) => {
                ctx.handle_get_header(params, res_tx)
            }

            // get_paylaod
            (State::Sorting(sorting), Event::GetPayload { blinded, block_hash, trace, res_tx }) => {
                ctx.handle_get_payload(block_hash, blinded, trace, res_tx, &sorting.slot);
            }

            // sim result
            (State::Sorting(sorting), Event::SimResult(result)) => {
                ctx.sort_simulation_result(&result);
                ctx.handle_simulation_result(result);
            }

            (State::Sorting(sorting), Event::GossipPayload(payload)) => {
                ctx.handle_gossip_payload(payload, &sorting.slot);
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
            (State::Slot { slot, .. }, Event::GossipPayload(payload)) => {
                warn!(curr =% slot, gossip_slot = payload.slot, "received early or late gossip payload");
            }
        }
    }

    fn tick(&mut self) {
        // DO we actually need a tick, could check on each gossip / submission instead
        // TODO: check pending payloads
    }
}
