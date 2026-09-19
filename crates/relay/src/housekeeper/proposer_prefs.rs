use helix_common::{
    ProposerDuty, ValidatorPreferences,
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    beacon::types::{ProposerPreferences, ProposerPreferencesEvent, SignedProposerPreferences},
    chain_info::ChainInfo,
};
use helix_types::{SignedValidatorRegistration, Slot};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub struct ProposerPreferencesStore {
    /// The signature is kept so the proposer's own key can be checked later, once
    /// the duty that names the proposer is to hand.
    by_slot: FxHashMap<Slot, SignedProposerPreferences>,
}

impl ProposerPreferencesStore {
    /// The proposer may resubmit up to an epoch ahead, so a later event wins.
    pub fn process(&mut self, head: Slot, event: ProposerPreferencesEvent) {
        let signed = event.data;
        if signed.message.proposal_slot <= head {
            return;
        }
        self.by_slot.insert(signed.message.proposal_slot, signed);
    }

    pub fn get(&self, slot: Slot) -> Option<&ProposerPreferences> {
        self.by_slot.get(&slot).map(|signed| &signed.message)
    }

    pub fn get_signed(&self, slot: Slot) -> Option<&SignedProposerPreferences> {
        self.by_slot.get(&slot)
    }

    pub fn on_new_slot(&mut self, bid_slot: Slot) {
        self.by_slot.retain(|slot, _| *slot >= bid_slot);
    }

    /// Whether the slot's preferences carry the proposer's own signature.
    pub fn check_signature(&self, duty: &ProposerDuty, chain_info: &ChainInfo) -> bool {
        let Some(signed) = self.get_signed(duty.slot) else {
            return false;
        };
        let ok = signed.verify(&duty.pubkey, chain_info);
        if !ok {
            tracing::warn!(
                slot = %duty.slot,
                validator_index = duty.validator_index,
                "proposer preferences failed signature verification",
            );
        }
        ok
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_slot.len()
    }
}

/// Whether these preferences were gossiped by the validator that actually proposes the
/// slot. Nothing else binds the two: the fee recipient our builder pays comes straight
/// from here, so preferences from any other index would redirect the payment.
pub fn prefs_match_duty(duty: &ProposerDuty, prefs: &ProposerPreferences) -> bool {
    prefs.validator_index == duty.validator_index && prefs.proposal_slot == duty.slot
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
    chain_info: &ChainInfo,
) -> Vec<BuilderGetValidatorsResponseEntry> {
    beacon_duties
        .iter()
        .filter(|duty| duty.slot >= from_slot)
        .filter(|duty| prefs.check_signature(duty, chain_info))
        .filter_map(|duty| {
            prefs
                .get(duty.slot)
                .filter(|prefs| prefs_match_duty(duty, prefs))
                .map(|prefs| synthesize_registration(duty, prefs, defaults))
        })
        .collect()
}

/// Merges freshly synthesized entries into the feed the builders poll.
///
/// A synthesized entry is only a snapshot of the last preferences gossiped for
/// the slot, and a proposer may resubmit up to an epoch ahead -- the store keeps
/// the latest on purpose. Keeping the first snapshot instead would leave
/// builders working from a stale fee recipient and gas limit. A real
/// registration, which a proposer signed, is never displaced.
pub fn merge_duty_feed(
    existing: Vec<BuilderGetValidatorsResponseEntry>,
    synthesized: Vec<BuilderGetValidatorsResponseEntry>,
) -> Vec<BuilderGetValidatorsResponseEntry> {
    let refreshed: FxHashMap<u64, BuilderGetValidatorsResponseEntry> =
        synthesized.into_iter().map(|entry| (entry.slot.as_u64(), entry)).collect();

    let mut feed: Vec<_> = existing
        .into_iter()
        .map(|entry| {
            let slot = entry.slot.as_u64();
            match refreshed.get(&slot) {
                Some(fresh) if !is_registered(&entry) => fresh.clone(),
                _ => entry,
            }
        })
        .collect();

    let known: rustc_hash::FxHashSet<u64> = feed.iter().map(|e| e.slot.as_u64()).collect();
    feed.extend(refreshed.into_values().filter(|entry| !known.contains(&entry.slot.as_u64())));
    feed.sort_by_key(|e| e.slot.as_u64());
    feed
}

/// Whether the proposer signed this registration itself, rather than it being
/// synthesized from gossiped preferences.
fn is_registered(entry: &BuilderGetValidatorsResponseEntry) -> bool {
    entry.entry.registration.signature != helix_types::BlsSignatureBytes::default()
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

    /// As `event`, but gossiped by `index` -- the feed only accepts preferences from
    /// the validator that actually proposes the slot.
    fn event_from(
        slot: u64,
        index: u64,
        fee_recipient: Address,
        target_gas_limit: u64,
    ) -> ProposerPreferencesEvent {
        let mut ev = event(slot, fee_recipient, target_gas_limit);
        ev.data.message.validator_index = index;
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

    /// Duties now have to carry a real key: the feed only accepts preferences the
    /// proposer actually signed.
    fn duty_signed_by(keypair: &helix_types::BlsKeypair, slot: u64, index: u64) -> ProposerDuty {
        ProposerDuty {
            pubkey: keypair.pk.serialize().into(),
            validator_index: index,
            slot: Slot::new(slot),
        }
    }

    fn sign_event(
        keypair: &helix_types::BlsKeypair,
        chain_info: &ChainInfo,
        mut ev: ProposerPreferencesEvent,
    ) -> ProposerPreferencesEvent {
        use helix_types::{EthSpec, MainnetEthSpec, SignedRoot};
        let epoch = ev.data.message.proposal_slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            helix_types::Domain::ProposerPreferences,
            &fork,
            chain_info.genesis_validators_root,
        );
        ev.data.signature =
            keypair.sk.sign(ev.data.message.signing_root(domain)).serialize().into();
        ev
    }

    /// Builds duties and a matching, properly signed store for `(slot, index)` pairs.
    fn duties_and_store(
        entries: &[(u64, u64)],
        chain_info: &ChainInfo,
    ) -> (Vec<ProposerDuty>, ProposerPreferencesStore) {
        helix_common::utils::install_default_crypto_provider();
        let mut duties = Vec::new();
        let mut store = ProposerPreferencesStore::default();
        for (slot, index) in entries {
            let keypair = helix_types::BlsKeypair::random();
            duties.push(duty_signed_by(&keypair, *slot, *index));
            let ev = event_from(
                *slot,
                *index,
                address!("00000000000000000000000000000000000000aa"),
                45_000_000,
            );
            store.process(Slot::new(0), sign_event(&keypair, chain_info, ev));
        }
        (duties, store)
    }

    #[test]
    fn serves_only_the_slots_with_a_gossiped_preference() {
        let chain_info = ChainInfo::default();
        let (mut duties, store) = duties_and_store(&[(101, 1), (103, 3)], &chain_info);
        duties.push(duty_signed_by(&helix_types::BlsKeypair::random(), 102, 2));
        duties.sort_by_key(|d| d.slot);

        let feed = synthesize_duty_feed(
            &duties,
            &store,
            Slot::new(101),
            &ValidatorPreferences::default(),
            &chain_info,
        );

        let slots: Vec<u64> = feed.iter().map(|e| e.slot.as_u64()).collect();
        assert_eq!(slots, vec![101, 103], "a slot without preferences cannot be served");
        assert_eq!(feed[0].validator_index, 1);
        assert_eq!(feed[1].validator_index, 3);
    }

    #[test]
    fn drops_duties_before_the_bid_slot() {
        let chain_info = ChainInfo::default();
        let (duties, store) = duties_and_store(&[(100, 1), (101, 2)], &chain_info);

        let feed = synthesize_duty_feed(
            &duties,
            &store,
            Slot::new(101),
            &ValidatorPreferences::default(),
            &chain_info,
        );

        let slots: Vec<u64> = feed.iter().map(|e| e.slot.as_u64()).collect();
        assert_eq!(slots, vec![101], "builders cannot build a slot that has started");
    }

    /// A proposer may resubmit its preferences, and the store keeps the latest on
    /// purpose. Keeping the first snapshot in the feed left builders working from a
    /// stale gas limit and fee recipient, which the proposer then rejects.
    #[test]
    fn a_resubmitted_preference_replaces_the_served_entry() {
        let chain_info = ChainInfo::default();
        let (duties, store) = duties_and_store(&[(101, 1)], &chain_info);
        let first = synthesize_duty_feed(
            &duties,
            &store,
            Slot::new(101),
            &ValidatorPreferences::default(),
            &chain_info,
        );
        assert_eq!(first[0].entry.registration.message.gas_limit, 45_000_000);

        // The proposer changes its mind about the gas limit.
        let mut later = first.clone();
        later[0].entry.registration.message.gas_limit = 200_000_000;

        let merged = merge_duty_feed(first, later);

        assert_eq!(merged.len(), 1, "the slot must not be duplicated");
        assert_eq!(
            merged[0].entry.registration.message.gas_limit, 200_000_000,
            "the builder has to see the latest preferences, not the first",
        );
    }

    /// A registration the proposer actually signed outranks anything synthesized.
    #[test]
    fn a_real_registration_is_not_displaced() {
        let chain_info = ChainInfo::default();
        let (duties, store) = duties_and_store(&[(101, 1)], &chain_info);
        let synthesized = synthesize_duty_feed(
            &duties,
            &store,
            Slot::new(101),
            &ValidatorPreferences::default(),
            &chain_info,
        );
        let mut registered = synthesized.clone();
        registered[0].entry.registration.signature =
            helix_types::BlsSignatureBytes::from([7u8; 96]);
        registered[0].entry.registration.message.gas_limit = 36_000_000;

        let merged = merge_duty_feed(registered, synthesized);

        assert_eq!(merged[0].entry.registration.message.gas_limit, 36_000_000);
    }

    /// The fee recipient the builder pays comes straight from these preferences, so
    /// preferences gossiped by anyone but the slot's proposer would redirect the
    /// payment. Nothing else binds the two.
    #[test]
    fn refuses_preferences_gossiped_by_another_validator() {
        helix_common::utils::install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let attacker_key = helix_types::BlsKeypair::random();
        let attacker_address = address!("00000000000000000000000000000000000000ff");
        let duties = [duty_signed_by(&helix_types::BlsKeypair::random(), 101, 1)];

        // Correctly signed, but by someone who does not propose this slot.
        let mut store = ProposerPreferencesStore::default();
        let ev = event_from(101, 999, attacker_address, 45_000_000);
        store.process(Slot::new(0), sign_event(&attacker_key, &chain_info, ev));

        let feed = synthesize_duty_feed(
            &duties,
            &store,
            Slot::new(101),
            &ValidatorPreferences::default(),
            &chain_info,
        );

        assert!(feed.is_empty(), "a slot must not be served on someone else's preferences");
    }
}
