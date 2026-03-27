use std::collections::HashMap;

use alloy_primitives::B256;
use helix_common::{PayloadAttributesUpdate, beacon::types::PayloadAttributesEvent};
use helix_types::Slot;
use tracing::info;
use tree_hash::TreeHash;

use crate::housekeeper::chain_head::ChainHead;

pub fn process_payload_attributes(
    chain_head: &mut ChainHead,
    event: PayloadAttributesEvent,
    known_payload_attributes: &mut HashMap<(B256, Slot), PayloadAttributesUpdate>,
) {
    // Drop stale payload attributes
    if chain_head.head() >= event.data.proposal_slot {
        return;
    }

    // Drop duplicates for the same parent and slot. We may receive multiple events for the same
    // slot
    let payload_attributes_key = (event.data.parent_block_hash, event.data.proposal_slot);
    if known_payload_attributes.contains_key(&payload_attributes_key) {
        return;
    }

    info!(
        head_slot =% chain_head.head(),
        payload_attribute_slot =% event.data.proposal_slot,
        payload_attribute_parent = ?event.data.parent_block_hash,
        "processing payload attribute event",
    );

    // Clear out any stale payload attributes for slots that have been passed
    known_payload_attributes.retain(|_, value| value.slot >= chain_head.head());

    let withdrawals_root = event.data.payload_attributes.withdrawals.tree_hash_root();
    let update = PayloadAttributesUpdate {
        slot: event.data.proposal_slot,
        parent_hash: event.data.parent_block_hash,
        withdrawals_root,
        payload_attributes: event.data.payload_attributes,
    };

    // Only mark done when the attrs are for the upcoming bid slot. An event for
    // a slot further ahead must not unblock the state machine prematurely.
    let is_bid_slot = update.slot == chain_head.head() + 1;
    known_payload_attributes.insert(payload_attributes_key, update);
    if is_bid_slot {
        chain_head.mark_payload_attrs_done();
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use alloy_primitives::B256;
    use helix_common::{
        beacon::types::{HeadEventData, PayloadAttributesEvent, PayloadAttributesEventData},
        chain_info::ChainInfo,
    };

    use super::*;
    use crate::housekeeper::chain_head::ChainHead;

    fn make_chain_head() -> (ChainHead, helix_types::Slot) {
        let ci = Arc::new(ChainInfo::default());
        let head_slot = ci.current_slot() + 1;
        let mut ch = ChainHead::new(ci);
        ch.update(HeadEventData { slot: head_slot, block: B256::ZERO, state: String::new() });
        (ch, head_slot)
    }

    fn make_event(proposal_slot: helix_types::Slot, parent_hash: B256) -> PayloadAttributesEvent {
        PayloadAttributesEvent {
            version: String::new(),
            data: PayloadAttributesEventData {
                proposal_slot,
                parent_block_hash: parent_hash,
                ..Default::default()
            },
        }
    }

    #[test]
    fn stale_event_dropped() {
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        // proposal_slot == head_slot: stale
        process_payload_attributes(&mut ch, make_event(head_slot, B256::ZERO), &mut known);
        assert!(known.is_empty());
    }

    #[test]
    fn duplicate_event_dropped() {
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        let slot = head_slot + 1;
        let parent = B256::from([1u8; 32]);
        process_payload_attributes(&mut ch, make_event(slot, parent), &mut known);
        assert_eq!(known.len(), 1);
        process_payload_attributes(&mut ch, make_event(slot, parent), &mut known);
        assert_eq!(known.len(), 1);
    }

    #[test]
    fn valid_event_stored() {
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        process_payload_attributes(
            &mut ch,
            make_event(head_slot + 1, B256::from([2u8; 32])),
            &mut known,
        );
        assert_eq!(known.len(), 1);
    }

    #[test]
    fn stale_entries_pruned_on_new_event() {
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        // Insert an entry for head_slot + 1
        process_payload_attributes(
            &mut ch,
            make_event(head_slot + 1, B256::from([3u8; 32])),
            &mut known,
        );
        assert_eq!(known.len(), 1);
        // Advance head past that slot
        ch.update(HeadEventData { slot: head_slot + 2, block: B256::ZERO, state: String::new() });
        // New event for head_slot + 3 should prune the old head_slot+1 entry
        process_payload_attributes(
            &mut ch,
            make_event(head_slot + 3, B256::from([4u8; 32])),
            &mut known,
        );
        // head_slot+1 < current head (head_slot+2), so it's pruned; only new entry remains
        assert_eq!(known.len(), 1);
    }

    // --- Payload attrs done flag ---

    #[test]
    fn bid_slot_event_marks_payload_attrs_done() {
        // proposal_slot == head + 1 (the bid slot): should mark done.
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        process_payload_attributes(
            &mut ch,
            make_event(head_slot + 1, B256::from([5u8; 32])),
            &mut known,
        );
        // Force all conditions; payload_attrs_done should already be true.
        ch.mark_duties_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
    }

    #[test]
    fn future_slot_event_does_not_mark_payload_attrs_done() {
        // proposal_slot == head + 2: stored in cache but must NOT mark done for bid_slot.
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        process_payload_attributes(
            &mut ch,
            make_event(head_slot + 2, B256::from([6u8; 32])),
            &mut known,
        );
        // Event stored.
        assert_eq!(known.len(), 1);
        // payload_attrs_done must NOT have been set, so is_ready returns false (deadline far off).
        ch.mark_duties_done();
        ch.mark_il_done();
        assert!(!ch.is_ready());
    }

    #[test]
    fn two_events_same_slot_different_parents_both_stored() {
        // Same proposal_slot but different parent → distinct keys, both kept.
        let (mut ch, head_slot) = make_chain_head();
        let mut known = HashMap::new();
        let slot = head_slot + 1;
        process_payload_attributes(&mut ch, make_event(slot, B256::from([7u8; 32])), &mut known);
        process_payload_attributes(&mut ch, make_event(slot, B256::from([8u8; 32])), &mut known);
        assert_eq!(known.len(), 2);
    }
}
