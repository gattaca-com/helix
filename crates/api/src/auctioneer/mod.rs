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
use helix_common::{chain_info::ChainInfo, local_cache::LocalCache, RelayConfig};
use helix_types::Slot;
pub use simulator::*;
use tokio::runtime;
use tracing::{debug, info, warn};
pub use types::GetPayloadResultData;

use crate::{
    auctioneer::{
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

    let sim_manager = SimulatorManager::new(config.simulators.clone(), event_tx.clone(), runtime);
    let ctx = Context::new(chain_info, config, sim_manager, db);
    let auctioneer = Auctioneer::<A> { ctx, state: State::Slot { slot: Default::default() } };

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
    /// Next proposer is registered and we are processing builder bids
    Sorting(SortingData),
    /// Processing get_payload, broadcasting block
    Broadcasting { slot_ctx: SlotContext, block_hash: B256 },
    /// Next proposer is not registered, reject most calls
    Slot { slot: Slot },
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
                ctx.sim_manager.handle_sync_status(id, is_synced);
            }

            // late sim results
            (State::Broadcasting { .. } | State::Slot { .. }, Event::SimResult(result)) => {
                ctx.handle_simulation_result(result);
            }

            ///////////// VALID STATES / EVENTS /////////////

            // submission
            (
                State::Sorting(sorting),
                Event::Submission { submission, withdrawals_root, sequence, trace, res_tx },
            ) => sorting.handle_submission(
                submission,
                withdrawals_root,
                sequence,
                trace,
                ctx,
                res_tx,
            ),

            // get_header
            (State::Sorting(sorting), Event::GetHeader { params, res_tx }) => {
                sorting.handle_get_header(params, res_tx)
            }

            // get_paylaod
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
