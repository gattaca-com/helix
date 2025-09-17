#![allow(unused)]

mod sorting;
mod validation;

use std::{collections::BTreeMap, ops::Deref, time::Instant};

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, bid_submission::BidSubmission,
    chain_info::ChainInfo, BuilderInfo,
};
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{BlsPublicKeyBytes, ForkName, SignedBidSubmission, SignedBuilderBid, Slot};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::oneshot;

use crate::{
    builder::{error::BuilderApiError, BlockSimRequest},
    proposer::GetHeaderParams,
};

pub struct Auctioneer {
    ctx: Context,
    state: State,
}

impl Auctioneer {
    fn run(mut self, rx: crossbeam_channel::Receiver<Event>) {
        loop {
            let Ok(evt) = rx.try_recv() else {
                continue;
            };

            self.state.step(evt, &mut self.ctx);
        }
    }
}

struct Context {
    chain_info: ChainInfo,
    builder_info: FxHashMap<BlsPublicKeyBytes, BuilderInfo>,
    unknown_builder_info: BuilderInfo,
    simulator: (),
}

struct SortingData {
    slot: SlotContext,
    sort: SortContext,
    seen_block_hashes: FxHashSet<B256>,
    sequence: FxHashMap<BlsPublicKeyBytes, u64>,
    get_header_queue:
        BTreeMap<Instant, oneshot::Sender<Result<SignedBuilderBid, ProposerApiError>>>,
}

enum State {
    /// Next proposer is registered and we are processing builder bids
    Sorting(SortingData),
    /// Processing get_payload, broadcasting block
    Broadcasting { slot_ctx: SlotContext, block_hash: B256 },
    /// Next proposer is not registered, rejecting bids
    Slot { slot: Slot },
}

enum ProposerApiError {}

enum Event {
    Submission {
        submission: NewSubmission,
        res_tx: oneshot::Sender<Result<(), BuilderApiError>>,
    },
    GetHeader {
        params: GetHeaderParams,
        res_tx: oneshot::Sender<Result<SignedBuilderBid, ProposerApiError>>,
    },
    GetPayload,
    NewSlot,
    SimResult,
    MergeResult,
}

impl State {
    fn step(&mut self, event: Event, ctx: &mut Context) {
        match self {
            State::Sorting(sorting) => match event {
                Event::Submission { submission, res_tx } => {
                    let builder_info = ctx
                        .builder_info
                        .get(submission.builder_public_key())
                        .unwrap_or(&ctx.unknown_builder_info);

                    match sorting.handle_submission(submission, builder_info) {
                        Ok(sim_request) => {
                            // add to sim manager queue
                        }

                        Err(err) => {
                            let _ = res_tx.send(Err(err));
                        }
                    };
                }

                Event::GetHeader { params, res_tx } => {
                    // let res =
                }
                Event::GetPayload => todo!(),
                Event::NewSlot => todo!(),
                _ => todo!(),
            },
            State::Broadcasting { slot_ctx, block_hash } => todo!(),
            State::Slot { slot } => todo!(),
        }
    }
}

impl SortingData {
    fn handle_get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<SignedBuilderBid, ProposerApiError> {
        todo!()
    }

    fn handle_submission(
        &mut self,
        submission: NewSubmission,
        builder_info: &BuilderInfo,
    ) -> Result<BlockSimRequest, BuilderApiError> {
        self.validate_submission(&submission, builder_info)?;

        if self.should_process_optimistically(&submission, builder_info) {
            self.sort.sort(&submission);
        }

        // TODO: inclusion list

        let sim_request = BlockSimRequest::new(
            self.slot.registration_data.entry.registration.message.gas_limit,
            &submission,
            self.slot.registration_data.entry.preferences.clone(),
            self.slot.payload_attributes.payload_attributes.parent_beacon_block_root,
            None,
        );

        Ok(sim_request)
    }

    fn should_process_optimistically(
        &self,
        submission: &NewSubmission,
        builder_info: &BuilderInfo,
    ) -> bool {
        if builder_info.is_optimistic && submission.message().value <= builder_info.collateral {
            if self.slot.registration_data.entry.preferences.filtering.is_regional() &&
                !builder_info.can_process_regional_slot_optimistically()
            {
                return false;
            }

            // if self.failsafe_triggered.load(Ordering::Relaxed) {
            //     return false;
            // }

            // if self.optimistic_state.is_paused() {
            //     return false;
            // }

            return true;
        }

        false
    }
}

// should we check that the bid_slot == next_duty slot
// do we need coordinates here
pub struct SlotContext {
    /// Builders are bidding to build this slot, head_slot + 1
    bid_slot: Slot,
    /// Data about the validator registration
    registration_data: BuilderGetValidatorsResponseEntry,
    /// Payload attributes for the incoming blocks
    payload_attributes: PayloadAttributesUpdate,
    /// Current fork
    current_fork: ForkName,
}

pub struct SortContext {}

// cpu intensive stuff is:
// parsing headers, decode, hydration, sig verify, withdrawals and tx root, mergeable orders,
// get payload stuff, re-signing get header
struct NewSubmission {
    payload: SignedBidSubmission,
    sequence: Option<u64>,
    withdrawals_root: B256,
    tx_root: B256,
}

impl Deref for NewSubmission {
    type Target = SignedBidSubmission;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}
