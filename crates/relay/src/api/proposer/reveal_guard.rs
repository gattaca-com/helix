use std::collections::BTreeMap;

use alloy_primitives::B256;
use helix_types::Slot;
use tokio::task::AbortHandle;

/// Slots kept after the current one, so a late duplicate for the previous slot is
/// still recognised.
const RETAINED_SLOTS: u64 = 2;

/// What [`RevealGuard::register`] decided about a block.
#[derive(Debug)]
pub enum Registered {
    /// The first block for this slot, or a repeat of the same one.
    Proceed,
    /// A second, different block for a slot we already hold one for.
    Equivocation { first: B256 },
}

/// Remembers the block root helix committed to for each recent slot, so a proposer
/// cannot redeem two different blocks against the same bid. A pending reveal is
/// abortable: the payload is held until the attestation deadline, so an equivocation
/// seen before then withholds it rather than merely recording the fact.
#[derive(Default)]
pub struct RevealGuard {
    seen: BTreeMap<Slot, (B256, Option<AbortHandle>)>,
}

impl RevealGuard {
    pub fn register(&mut self, slot: Slot, block_root: B256) -> Registered {
        self.prune(slot);
        match self.seen.get(&slot) {
            Some((first, _)) if *first != block_root => Registered::Equivocation { first: *first },
            Some(_) => Registered::Proceed,
            None => {
                self.seen.insert(slot, (block_root, None));
                Registered::Proceed
            }
        }
    }

    /// Attaches the task that will reveal `slot`'s payload, so it can be withheld.
    pub fn attach(&mut self, slot: Slot, reveal: AbortHandle) {
        if let Some(entry) = self.seen.get_mut(&slot) {
            entry.1 = Some(reveal);
        }
    }

    /// Withholds `slot`'s payload if its reveal has not run yet. Returns whether a
    /// pending reveal was stopped.
    pub fn withhold(&mut self, slot: Slot) -> bool {
        match self.seen.get_mut(&slot).and_then(|entry| entry.1.take()) {
            Some(reveal) if !reveal.is_finished() => {
                reveal.abort();
                true
            }
            _ => false,
        }
    }

    fn prune(&mut self, slot: Slot) {
        let cutoff = slot.as_u64().saturating_sub(RETAINED_SLOTS);
        self.seen.retain(|kept, _| kept.as_u64() >= cutoff);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use super::*;

    fn guard() -> RevealGuard {
        RevealGuard::default()
    }

    #[test]
    fn the_first_block_for_a_slot_proceeds() {
        let mut guard = guard();
        assert!(matches!(guard.register(Slot::new(10), B256::repeat_byte(1)), Registered::Proceed));
    }

    #[test]
    fn the_same_block_sent_twice_is_not_an_equivocation() {
        let mut guard = guard();
        let root = B256::repeat_byte(1);
        guard.register(Slot::new(10), root);
        assert!(matches!(guard.register(Slot::new(10), root), Registered::Proceed));
    }

    #[test]
    fn a_different_block_for_the_same_slot_is_an_equivocation() {
        let mut guard = guard();
        let first = B256::repeat_byte(1);
        guard.register(Slot::new(10), first);

        match guard.register(Slot::new(10), B256::repeat_byte(2)) {
            Registered::Equivocation { first: reported } => assert_eq!(reported, first),
            other => panic!("expected an equivocation, got {other:?}"),
        }
    }

    #[test]
    fn different_slots_do_not_collide() {
        let mut guard = guard();
        guard.register(Slot::new(10), B256::repeat_byte(1));
        assert!(matches!(guard.register(Slot::new(11), B256::repeat_byte(2)), Registered::Proceed));
    }

    /// Without pruning the map grows without bound; a slot far enough back is
    /// forgotten, so its root can no longer be compared.
    #[test]
    fn slots_well_behind_the_head_are_forgotten() {
        let mut guard = guard();
        guard.register(Slot::new(10), B256::repeat_byte(1));
        guard.register(Slot::new(20), B256::repeat_byte(2));
        assert!(matches!(guard.register(Slot::new(10), B256::repeat_byte(3)), Registered::Proceed));
    }

    #[tokio::test]
    async fn an_equivocation_withholds_a_pending_reveal() {
        let mut guard = guard();
        let slot = Slot::new(10);
        guard.register(slot, B256::repeat_byte(1));

        let revealed = Arc::new(AtomicBool::new(false));
        let flag = revealed.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flag.store(true, Ordering::SeqCst);
        });
        guard.attach(slot, task.abort_handle());

        assert!(matches!(
            guard.register(slot, B256::repeat_byte(2)),
            Registered::Equivocation { .. }
        ));
        assert!(guard.withhold(slot), "the pending reveal must be stopped");

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!revealed.load(Ordering::SeqCst), "the payload must never be revealed");
    }

    #[tokio::test]
    async fn a_reveal_that_already_ran_cannot_be_withheld() {
        let mut guard = guard();
        let slot = Slot::new(10);
        guard.register(slot, B256::repeat_byte(1));

        let task = tokio::spawn(async {});
        guard.attach(slot, task.abort_handle());
        let _ = task.await;

        assert!(!guard.withhold(slot));
    }
}
