#![allow(unused)]

use alloy_primitives::B256;
use helix_common::api::builder_api::BuilderGetValidatorsResponseEntry;
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{SignedBidSubmission, Slot};

use crate::builder::error::BuilderApiError;

pub struct Auctioneer {}

pub struct SlotContext {
    /// Builders are bidding to build this slot, head_slot + 1
    bid_slot: Slot,
    /// Data about the validator registration
    registration_data: BuilderGetValidatorsResponseEntry,
    /// Payload attributes for the incoming blocks
    payload_attributes: PayloadAttributesUpdate,
}

pub struct SortingContext {}

enum State {
    /// Next proposer is registered and we are processing builder bids
    Sorting { slot_ctx: SlotContext, sort_ctx: SortingContext },
    /// Processing get_payload, broadcasting block
    Broadcasting { block_hash: B256 },
    /// Next proposer is not registered, no point in receiving
    Slot {},
}

// cpu intensive stuff is:
// decode, hydration, sig verify, withdrawals and tx root, mergeable orders
struct NewSubmission {
    payload: SignedBidSubmission,
    withdrawals_root: B256,
    tx_root: B256,
}

impl Auctioneer {
    fn handle_submission(
        submission: NewSubmission,
        slot_ctx: &SlotContext,
        sort_ctx: &mut SortingContext,
    ) -> Result<(), BuilderApiError> {
        Ok(())
    }
}
