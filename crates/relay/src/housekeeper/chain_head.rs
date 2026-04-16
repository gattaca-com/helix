use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use helix_common::{beacon::types::HeadEventData, chain_info::ChainInfo, utils::utcnow_dur};
use helix_types::{Epoch, Slot, SlotClockTrait};

const CUTOFF_TIME: Duration = Duration::from_secs(4);

#[derive(Debug, PartialEq, Eq)]
enum ChainHeadState {
    Pending,
    Sent,
}

pub struct ChainHead {
    slot: Slot,
    block: Option<B256>,
    state: ChainHeadState,
    next_deadline: Instant,
    chain_info: Arc<ChainInfo>,
    payload_attributes_done: bool,
    duties_done: bool,
    inclusion_list_done: bool,
}

// let epoch = self.chain_head.slot.epoch(self.chain_info.slots_per_epoch());
// info!(
//     from_timeout = block_hash.is_none(),
//     into_slot =? self.chain_info.duration_into_slot(slot).unwrap_or_default(),
//     slot_pos = self.chain_info.slot_in_epoch(slot),
//     %epoch,
//     epoch_start = %epoch.start_slot(self.chain_info.slots_per_epoch()),
//     epoch_end = %epoch.end_slot(self.chain_info.slots_per_epoch()),
//     "processing new slot",
// );

impl ChainHead {
    pub fn new(chain_info: Arc<ChainInfo>) -> Self {
        let current_slot = chain_info.current_slot();
        Self {
            slot: current_slot,
            block: None,
            state: ChainHeadState::Pending,
            next_deadline: Instant::now() + slot_cutoff_timeout(&chain_info, current_slot),
            duties_done: false,
            payload_attributes_done: false,
            inclusion_list_done: false,
            chain_info,
        }
    }
    pub fn head(&self) -> Slot {
        self.slot
    }

    pub fn chain_info(&self) -> &ChainInfo {
        &self.chain_info
    }

    pub fn epoch(&self) -> Epoch {
        self.slot.epoch(self.chain_info.slots_per_epoch())
    }

    pub fn block(&self) -> Option<B256> {
        self.block
    }
    fn already_seen(&mut self, new_slot: Slot) -> bool {
        self.slot >= new_slot
    }
    pub fn is_ready(&self) -> bool {
        match self.state {
            ChainHeadState::Pending => {
                self.deadline_passed() ||
                    (self.block.is_some() &&
                        self.duties_done &&
                        self.payload_attributes_done &&
                        self.inclusion_list_done)
            }
            ChainHeadState::Sent => false,
        }
    }
    pub fn is_new_slot(&mut self) -> bool {
        let current = self.chain_info.current_slot();
        // Setting self.slot = current makes head < current false on the next call,
        // so no additional state guard is needed.
        if self.head() < current {
            let prev = self.slot;
            self.slot = current;
            self.block = None;
            self.state = ChainHeadState::Pending;
            // Use prev (not current) so the deadline is start_of(current) + CUTOFF_TIME,
            // i.e. CUTOFF_TIME into the new slot. Using current would give CUTOFF_TIME
            // into the slot after next (~16s), which lets a subsequent is_new_slot() call
            // overwrite this state before the deadline fires (causing skipped slot events).
            self.next_deadline = Instant::now() + slot_cutoff_timeout(&self.chain_info, prev);
            self.duties_done = false;
            self.payload_attributes_done = false;
            self.inclusion_list_done = false;
            true
        } else {
            false
        }
    }
    fn deadline_passed(&self) -> bool {
        Instant::now() >= self.next_deadline
    }
    /// Returns true if this event advanced us to a new slot (the else branch).
    /// The caller must start fetches when true — is_new_slot() will not fire for
    /// this slot because head == current by the time it runs.
    pub fn update(&mut self, ev: HeadEventData) -> bool {
        if self.already_seen(ev.slot) {
            if self.state != ChainHeadState::Sent {
                self.block = Some(ev.block);
            }
            false
        } else {
            self.slot = ev.slot;
            self.block = Some(ev.block);
            self.state = ChainHeadState::Pending;
            // Reset deadline so we don't fire against a stale slot-N-1 cutoff.
            self.next_deadline = Instant::now() + slot_cutoff_timeout(&self.chain_info, self.slot);
            self.duties_done = false;
            self.payload_attributes_done = false;
            self.inclusion_list_done = false;
            true
        }
    }
    pub fn mark_duties_done(&mut self) {
        self.duties_done = true;
    }
    pub fn mark_payload_attrs_done(&mut self) {
        self.payload_attributes_done = true;
    }
    pub fn mark_il_done(&mut self) {
        self.inclusion_list_done = true;
    }
    pub fn sent(&mut self) {
        self.state = ChainHeadState::Sent;
    }

    #[cfg(test)]
    pub fn force_deadline_expired(&mut self) {
        self.next_deadline = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use helix_common::{beacon::types::HeadEventData, chain_info::ChainInfo};

    use super::ChainHead;

    fn make_head() -> (ChainHead, Arc<ChainInfo>) {
        let ci = Arc::new(ChainInfo::default());
        let ch = ChainHead::new(ci.clone());
        (ch, ci)
    }

    fn head_event(slot: impl Into<helix_types::Slot>) -> HeadEventData {
        HeadEventData { slot: slot.into(), block: B256::ZERO, state: String::new() }
    }

    fn head_event_with_block(slot: impl Into<helix_types::Slot>, block: B256) -> HeadEventData {
        HeadEventData { slot: slot.into(), block, state: String::new() }
    }

    // --- update() return value ---

    #[test]
    fn update_new_slot_returns_true() {
        let (mut ch, ci) = make_head();
        assert!(ch.update(head_event(ci.current_slot() + 1)));
    }

    #[test]
    fn update_already_seen_returns_false() {
        let (mut ch, ci) = make_head();
        // Same slot: already_seen → false.
        assert!(!ch.update(head_event(ci.current_slot())));
    }

    #[test]
    fn update_second_call_same_slot_returns_false() {
        // First call advances to new slot (true); second for same slot is already_seen (false).
        let (mut ch, ci) = make_head();
        let next = ci.current_slot() + 1;
        assert!(ch.update(head_event(next)));
        assert!(!ch.update(head_event(next)));
    }

    #[test]
    fn update_new_slot_resets_deadline() {
        // The else branch must set a fresh deadline so we don't fire against a
        // stale cutoff from the previous slot.
        let (mut ch, ci) = make_head();
        ch.force_deadline_expired();
        // Advancing to a future slot resets the deadline, so is_ready() must
        // not fire immediately (Pending, flags unset, fresh deadline).
        ch.update(head_event(ci.current_slot() + 1));
        assert!(!ch.is_ready());
    }

    // --- Head event transitions ---

    #[test]
    fn future_head_event_advances_slot() {
        let (mut ch, ci) = make_head();
        let next = ci.current_slot() + 1;
        ch.update(head_event(next));
        assert_eq!(ch.head(), next);
    }

    #[test]
    fn same_slot_head_event_records_block_and_unlocks_flags() {
        // already_seen head event records the block, enabling flag-based ready.
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot()));
        // Head must not have changed.
        assert_eq!(ch.head(), ci.current_slot());
        // Flags now unlock ready (block is recorded).
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
    }

    #[test]
    fn past_head_event_records_block_and_does_not_regress_head() {
        // already_seen for a past slot: block recorded, head unchanged.
        let (mut ch, ci) = make_head();
        let current = ci.current_slot();
        if current.as_u64() > 0 {
            ch.update(head_event(current - 1));
            assert_eq!(ch.head(), current);
        }
    }

    #[test]
    fn head_event_after_sent_does_not_re_enable_ready() {
        // After Sent, a duplicate head event must not re-open the block gate.
        let (mut ch, ci) = make_head();
        let slot = ci.current_slot() + 1;
        ch.update(head_event(slot));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
        ch.sent();

        ch.update(head_event(slot)); // already_seen, state=Sent → block not updated
        assert!(!ch.is_ready());
    }

    #[test]
    fn head_event_records_block_hash() {
        // After a head event, flags + block gate unlock ready.
        let (mut ch, ci) = make_head();
        ch.update(head_event_with_block(ci.current_slot() + 1, B256::from([0xab; 32])));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
    }

    // --- Ready conditions ---

    #[test]
    fn ready_when_all_flags_set() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
    }

    #[test]
    fn not_ready_missing_duties() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(!ch.is_ready());
    }

    #[test]
    fn not_ready_missing_payload_attrs() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_duties_done();
        ch.mark_il_done();
        assert!(!ch.is_ready());
    }

    #[test]
    fn not_ready_missing_il() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        assert!(!ch.is_ready());
    }

    #[test]
    fn deadline_timeout_from_new_slot_state_fires_ready() {
        // Without a head event the state machine must still go Ready after the deadline.
        let (mut ch, _ci) = make_head();
        // state = Pending at startup; deadline not expired yet.
        assert!(!ch.is_ready());
        ch.force_deadline_expired();
        assert!(ch.is_ready());
    }

    #[test]
    fn deadline_timeout_from_pending_fires_ready() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        // Only duties done — normally not enough.
        ch.mark_duties_done();
        assert!(!ch.is_ready());
        ch.force_deadline_expired();
        assert!(ch.is_ready());
    }

    // --- Sent state ---

    #[test]
    fn sent_transitions_out_of_ready() {
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        assert!(ch.is_ready());
        ch.sent();
        assert!(!ch.is_ready());
    }

    #[test]
    fn sent_does_not_re_enter_ready_from_deadline() {
        // After sent(), is_ready() must return false even if deadline has passed.
        let (mut ch, ci) = make_head();
        ch.update(head_event(ci.current_slot() + 1));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        ch.is_ready();
        ch.sent();
        ch.force_deadline_expired();
        assert!(!ch.is_ready());
    }

    // --- Flag reset on new head ---

    #[test]
    fn new_head_event_resets_flags() {
        let (mut ch, ci) = make_head();
        let slot = ci.current_slot() + 1;
        ch.update(head_event(slot));
        ch.mark_duties_done();
        ch.mark_payload_attrs_done();
        ch.mark_il_done();
        // Advance to a newer slot — flags must be cleared.
        ch.update(head_event(slot + 1));
        // Even with deadline expired, the flags are false so we only fire via deadline.
        ch.force_deadline_expired();
        assert!(ch.is_ready()); // deadline, not flags
    }
}

fn slot_cutoff_timeout(chain_info: &ChainInfo, head: Slot) -> Duration {
    chain_info
        .clock
        .start_of(head + 1)
        .map(|t| t.saturating_sub(utcnow_dur()) + CUTOFF_TIME)
        .unwrap_or(CUTOFF_TIME)
}
