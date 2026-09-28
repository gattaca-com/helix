use helix_types::BlsPublicKeyBytes;
use rustc_hash::FxHashMap;

/// Per-proposer-per-slot `max_execution_payment` preferences, submitted via
/// `submitBuilderPreferences` up to an epoch ahead of the slot they apply to. Lives on `Context`
/// (not `SlotContext`), since entries must survive across slot transitions until their own slot
/// arrives or passes.
#[derive(Default)]
pub struct BuilderPreferencesStore {
    by_proposer_slot: FxHashMap<(BlsPublicKeyBytes, u64), u64>,
}

impl BuilderPreferencesStore {
    pub fn store(
        &mut self,
        proposer_pubkey: BlsPublicKeyBytes,
        slot: u64,
        max_execution_payment: u64,
    ) {
        self.by_proposer_slot.insert((proposer_pubkey, slot), max_execution_payment);
    }

    pub fn max_execution_payment(
        &self,
        proposer_pubkey: &BlsPublicKeyBytes,
        slot: u64,
    ) -> Option<u64> {
        self.by_proposer_slot.get(&(*proposer_pubkey, slot)).copied()
    }

    /// Drops entries for slots that have already passed, so a proposer's own resubmissions
    /// (or one that never proposes) don't grow this unboundedly.
    pub fn on_new_slot(&mut self, bid_slot: u64) {
        self.by_proposer_slot.retain(|(_, slot), _| *slot >= bid_slot);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_proposer_slot.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pubkey(byte: u8) -> BlsPublicKeyBytes {
        BlsPublicKeyBytes::repeat_byte(byte)
    }

    #[test]
    fn stores_and_looks_up_per_proposer_per_slot() {
        let mut store = BuilderPreferencesStore::default();
        let alice = pubkey(1);
        let bob = pubkey(2);

        store.store(alice, 100, 500);
        store.store(bob, 100, 900);
        store.store(alice, 101, 700);

        assert_eq!(store.max_execution_payment(&alice, 100), Some(500));
        assert_eq!(store.max_execution_payment(&bob, 100), Some(900));
        assert_eq!(store.max_execution_payment(&alice, 101), Some(700));
        assert_eq!(store.max_execution_payment(&alice, 102), None);
    }

    #[test]
    fn resubmission_overwrites_the_previous_value() {
        let mut store = BuilderPreferencesStore::default();
        let alice = pubkey(1);

        store.store(alice, 100, 500);
        store.store(alice, 100, 600);

        assert_eq!(store.max_execution_payment(&alice, 100), Some(600));
    }

    #[test]
    fn on_new_slot_prunes_entries_for_slots_already_passed() {
        let mut store = BuilderPreferencesStore::default();
        let alice = pubkey(1);

        store.store(alice, 100, 500);
        store.store(alice, 101, 600);
        store.store(alice, 102, 700);

        store.on_new_slot(101);

        assert_eq!(store.max_execution_payment(&alice, 100), None);
        assert_eq!(store.max_execution_payment(&alice, 101), Some(600));
        assert_eq!(store.max_execution_payment(&alice, 102), Some(700));
        assert_eq!(store.len(), 2);
    }
}
