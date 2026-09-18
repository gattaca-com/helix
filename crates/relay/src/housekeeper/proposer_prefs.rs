use helix_common::{
    ProposerDuty, ValidatorPreferences,
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    beacon::types::{ProposerPreferences, ProposerPreferencesEvent},
};
use helix_types::{SignedValidatorRegistration, Slot};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub struct ProposerPreferencesStore {
    by_slot: FxHashMap<Slot, ProposerPreferences>,
}

impl ProposerPreferencesStore {
    /// The proposer may resubmit up to an epoch ahead, so a later event wins.
    pub fn process(&mut self, head: Slot, event: ProposerPreferencesEvent) {
        let prefs = event.data.message;
        if prefs.proposal_slot <= head {
            return;
        }
        self.by_slot.insert(prefs.proposal_slot, prefs);
    }

    pub fn get(&self, slot: Slot) -> Option<&ProposerPreferences> {
        self.by_slot.get(&slot)
    }

    pub fn on_new_slot(&mut self, bid_slot: Slot) {
        self.by_slot.retain(|slot, _| *slot >= bid_slot);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_slot.len()
    }
}

/// Gloas has no `registerValidator`, so the entry comes from the beacon duty and the gossiped
/// preferences, with this relay's configured preferences as the proposer cannot express its own.
pub fn synthesize_registration(
    duty: &ProposerDuty,
    prefs: &ProposerPreferences,
    defaults: &ValidatorPreferences,
) -> BuilderGetValidatorsResponseEntry {
    let mut registration = SignedValidatorRegistration::default();
    registration.message.pubkey = duty.pubkey;
    registration.message.fee_recipient = prefs.fee_recipient;
    registration.message.gas_limit = prefs.target_gas_limit;

    BuilderGetValidatorsResponseEntry {
        slot: duty.slot,
        validator_index: duty.validator_index,
        entry: ValidatorRegistrationInfo { registration, preferences: defaults.clone() },
    }
}

/// The duty feed builders poll. Gloas has no registrations, so a slot is served only once its
/// proposer has gossiped preferences for it.
pub fn synthesize_duty_feed(
    beacon_duties: &[ProposerDuty],
    prefs: &ProposerPreferencesStore,
    from_slot: Slot,
    defaults: &ValidatorPreferences,
) -> Vec<BuilderGetValidatorsResponseEntry> {
    beacon_duties
        .iter()
        .filter(|duty| duty.slot >= from_slot)
        .filter_map(|duty| {
            prefs.get(duty.slot).map(|prefs| synthesize_registration(duty, prefs, defaults))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, address};

    use super::*;

    const LIVE_EVENT: &str = r#"{
        "version": "gloas",
        "data": {
            "message": {
                "dependent_root": "0x0771be60148934328db0a9078a555c7ce7885f54a34d8c3c998ab7aafc5dfc39",
                "proposal_slot": "254171",
                "validator_index": "47100",
                "fee_recipient": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
                "target_gas_limit": "200000000"
            },
            "signature": "0xb36623b3b48d160aeaa1f088d0a60877283f32f0ff1dd375e351c0f600c56f9c888abe926258dabfeff3c67a9818955e0c2e48ce8b4a15ec59bb56cf6e9a95029df258a787191eec7e87d8d5996f8c1e4ec4524b3a37b6f05d5f78ce50d3eec9"
        }
    }"#;

    fn event(slot: u64, fee_recipient: Address, target_gas_limit: u64) -> ProposerPreferencesEvent {
        let mut ev: ProposerPreferencesEvent = serde_json::from_str(LIVE_EVENT).unwrap();
        ev.data.message.proposal_slot = Slot::new(slot);
        ev.data.message.fee_recipient = fee_recipient;
        ev.data.message.target_gas_limit = target_gas_limit;
        ev
    }

    #[test]
    fn parses_a_live_proposer_preferences_event() {
        let ev: ProposerPreferencesEvent = serde_json::from_str(LIVE_EVENT).unwrap();

        assert_eq!(ev.data.message.proposal_slot, Slot::new(254171));
        assert_eq!(ev.data.message.validator_index, 47100);
        assert_eq!(
            ev.data.message.fee_recipient,
            address!("f97e180c050e5ab072211ad2c213eb5aee4df134")
        );
        assert_eq!(ev.data.message.target_gas_limit, 200_000_000);
        assert_eq!(
            ev.data.message.dependent_root,
            B256::from_slice(&alloy_primitives::hex!(
                "0771be60148934328db0a9078a555c7ce7885f54a34d8c3c998ab7aafc5dfc39"
            ))
        );
    }

    #[test]
    fn synthesizes_the_entry_from_the_duty_and_the_gossiped_preferences() {
        let duty = ProposerDuty {
            pubkey: helix_types::BlsPublicKeyBytes::from([9u8; 48]),
            validator_index: 85292,
            slot: Slot::new(101),
        };
        let fee_recipient = address!("00000000000000000000000000000000000000aa");
        let prefs = ProposerPreferences {
            fee_recipient,
            target_gas_limit: 45_000_000,
            ..Default::default()
        };
        let defaults = ValidatorPreferences { header_delay: false, ..Default::default() };

        let entry = synthesize_registration(&duty, &prefs, &defaults);

        assert_eq!(entry.slot, Slot::new(101));
        assert_eq!(entry.validator_index, 85292);
        assert_eq!(entry.entry.registration.message.pubkey, duty.pubkey, "from the beacon duty");
        assert_eq!(entry.entry.registration.message.fee_recipient, fee_recipient, "from gossip");
        assert_eq!(entry.entry.registration.message.gas_limit, 45_000_000, "from gossip");
        assert!(!entry.entry.preferences.header_delay, "the relay's own preferences apply");
    }

    #[test]
    fn keeps_the_preference_for_a_future_slot() {
        let mut store = ProposerPreferencesStore::default();
        let fee_recipient = address!("00000000000000000000000000000000000000aa");

        store.process(Slot::new(100), event(101, fee_recipient, 45_000_000));

        let prefs = store.get(Slot::new(101)).expect("a future slot must be kept");
        assert_eq!(prefs.fee_recipient, fee_recipient);
        assert_eq!(prefs.target_gas_limit, 45_000_000);
        assert_eq!(prefs.validator_index, 47100);
    }

    #[test]
    fn drops_a_preference_for_a_passed_slot() {
        let mut store = ProposerPreferencesStore::default();
        let fee_recipient = address!("00000000000000000000000000000000000000aa");

        store.process(Slot::new(100), event(100, fee_recipient, 45_000_000));

        assert_eq!(store.len(), 0, "the relay cannot bid for a slot that has started");
    }

    #[test]
    fn a_later_event_replaces_the_slots_preference() {
        let mut store = ProposerPreferencesStore::default();
        let first = address!("00000000000000000000000000000000000000aa");
        let second = address!("00000000000000000000000000000000000000bb");

        store.process(Slot::new(100), event(101, first, 45_000_000));
        store.process(Slot::new(100), event(101, second, 60_000_000));

        let prefs = store.get(Slot::new(101)).expect("the slot must still be known");
        assert_eq!(prefs.fee_recipient, second, "the proposer's latest word wins");
        assert_eq!(prefs.target_gas_limit, 60_000_000);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn prunes_passed_slots_on_a_new_slot() {
        let mut store = ProposerPreferencesStore::default();
        let fee_recipient = address!("00000000000000000000000000000000000000aa");

        store.process(Slot::new(100), event(101, fee_recipient, 45_000_000));
        store.process(Slot::new(100), event(102, fee_recipient, 45_000_000));
        store.on_new_slot(Slot::new(102));

        assert!(store.get(Slot::new(101)).is_none(), "slot 101 has passed");
        assert!(store.get(Slot::new(102)).is_some(), "slot 102 is the bid slot");
        assert_eq!(store.len(), 1);
    }

    fn duty(slot: u64, index: u64) -> ProposerDuty {
        ProposerDuty {
            pubkey: helix_types::BlsPublicKeyBytes::from([index as u8; 48]),
            validator_index: index,
            slot: Slot::new(slot),
        }
    }

    fn store_with(slots: &[u64]) -> ProposerPreferencesStore {
        let mut store = ProposerPreferencesStore::default();
        for slot in slots {
            store.process(
                Slot::new(0),
                event(*slot, address!("00000000000000000000000000000000000000aa"), 45_000_000),
            );
        }
        store
    }

    #[test]
    fn serves_only_the_slots_with_a_gossiped_preference() {
        let duties = [duty(101, 1), duty(102, 2), duty(103, 3)];
        let store = store_with(&[101, 103]);

        let feed =
            synthesize_duty_feed(&duties, &store, Slot::new(101), &ValidatorPreferences::default());

        let slots: Vec<u64> = feed.iter().map(|e| e.slot.as_u64()).collect();
        assert_eq!(slots, vec![101, 103], "a slot without preferences cannot be served");
        assert_eq!(feed[0].validator_index, 1);
        assert_eq!(feed[1].validator_index, 3);
    }

    #[test]
    fn drops_duties_before_the_bid_slot() {
        let duties = [duty(100, 1), duty(101, 2)];
        let store = store_with(&[100, 101]);

        let feed =
            synthesize_duty_feed(&duties, &store, Slot::new(101), &ValidatorPreferences::default());

        let slots: Vec<u64> = feed.iter().map(|e| e.slot.as_u64()).collect();
        assert_eq!(slots, vec![101], "builders cannot build a slot that has started");
    }
}
