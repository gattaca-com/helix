use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponse, beacon::types::PayloadAttributesEvent,
};
use helix_types::{BlsPublicKeyBytes, Withdrawals};
use tracing::debug;

/// The proposer's registration for one slot, as the relay reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposerDuty {
    pub pubkey: BlsPublicKeyBytes,
    pub fee_recipient: Address,
    pub gas_limit: u64,
}

/// Everything needed to build and bid for one slot. The consensus fields come
/// from the beacon node, the proposer fields from the relay.
// Read once the block is assembled.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SlotContext {
    pub slot: u64,
    pub parent_hash: B256,
    pub parent_block_number: u64,
    pub timestamp: u64,
    pub prev_randao: B256,
    pub withdrawals: Withdrawals,
    pub parent_beacon_block_root: B256,
    pub proposer_pubkey: BlsPublicKeyBytes,
    pub proposer_fee_recipient: Address,
    pub registered_gas_limit: u64,
}

/// Merges the beacon node's `payload_attributes` events with the relay's
/// proposer duties, and decides which events are worth building for.
#[derive(Debug, Default)]
pub struct SlotTracker {
    duties: HashMap<u64, ProposerDuty>,
    /// Highest slot seen, for discarding replays after an SSE reconnect.
    latest_slot: u64,
    /// Slot and parent pairs already built for. The beacon node repeats an
    /// event whenever it recomputes the attributes.
    built: HashSet<(u64, B256)>,
}

impl SlotTracker {
    pub fn on_duties(&mut self, duties: Vec<BuilderGetValidatorsResponse>) {
        self.duties = duties
            .into_iter()
            .map(|duty| {
                let registration = duty.entry.message;
                (duty.slot.as_u64(), ProposerDuty {
                    pubkey: registration.pubkey,
                    fee_recipient: registration.fee_recipient,
                    gas_limit: registration.gas_limit,
                })
            })
            .collect();
        let latest_slot = self.latest_slot;
        self.duties.retain(|slot, _| *slot >= latest_slot);
    }

    #[cfg(test)]
    pub fn duty(&self, slot: u64) -> Option<&ProposerDuty> {
        self.duties.get(&slot)
    }

    pub fn on_payload_attributes(&mut self, event: PayloadAttributesEvent) -> Option<SlotContext> {
        let data = event.data;
        let slot = data.proposal_slot.as_u64();

        if slot < self.latest_slot {
            debug!(slot, latest = self.latest_slot, "discarding a stale payload_attributes event");
            return None;
        }
        if slot > self.latest_slot {
            self.latest_slot = slot;
            self.built.retain(|(built_slot, _)| *built_slot >= slot);
            self.duties.retain(|duty_slot, _| *duty_slot >= slot);
        }

        // EIP-4788 needs the root, so a block cannot be built without it.
        let Some(parent_beacon_block_root) = data.payload_attributes.parent_beacon_block_root
        else {
            debug!(slot, "skipping a slot with no parent_beacon_block_root");
            return None;
        };

        let Some(duty) = self.duties.get(&slot) else {
            debug!(slot, "skipping a slot with no registered proposer");
            return None;
        };

        if !self.built.insert((slot, data.parent_block_hash)) {
            return None;
        }

        Some(SlotContext {
            slot,
            parent_hash: data.parent_block_hash,
            parent_block_number: data.parent_block_number,
            timestamp: data.payload_attributes.timestamp,
            prev_randao: data.payload_attributes.prev_randao,
            withdrawals: data.payload_attributes.withdrawals,
            parent_beacon_block_root,
            // Never the event's `suggested_fee_recipient`: that is the local
            // validator's, not the proposer's.
            proposer_pubkey: duty.pubkey,
            proposer_fee_recipient: duty.fee_recipient,
            registered_gas_limit: duty.gas_limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use helix_common::beacon::types::{PayloadAttributes, PayloadAttributesEventData};

    use super::*;

    const PROPOSER_FEE_RECIPIENT: Address = Address::repeat_byte(0xaa);
    const LOCAL_FEE_RECIPIENT: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const REGISTERED_GAS_LIMIT: u64 = 30_000_000;

    fn duty_response(slot: u64) -> BuilderGetValidatorsResponse {
        let mut entry = helix_types::SignedValidatorRegistration::default();
        entry.message.fee_recipient = PROPOSER_FEE_RECIPIENT;
        entry.message.gas_limit = REGISTERED_GAS_LIMIT;
        entry.message.pubkey = BlsPublicKeyBytes::from([7u8; 48]);

        BuilderGetValidatorsResponse {
            slot: slot.into(),
            validator_index: 1,
            entry,
            preferences: Default::default(),
        }
    }

    fn event(slot: u64, parent: B256) -> PayloadAttributesEvent {
        PayloadAttributesEvent {
            version: "fulu".to_string(),
            data: PayloadAttributesEventData {
                proposer_index: 1,
                proposal_slot: slot.into(),
                parent_block_number: slot - 1,
                parent_block_root: String::new(),
                parent_block_hash: parent,
                payload_attributes: PayloadAttributes {
                    timestamp: 1_700_000_000 + slot * 12,
                    prev_randao: B256::repeat_byte(0xcc),
                    suggested_fee_recipient: LOCAL_FEE_RECIPIENT.to_string(),
                    withdrawals: Withdrawals::default(),
                    parent_beacon_block_root: Some(B256::repeat_byte(0xdd)),
                },
            },
        }
    }

    fn tracker_with_duty(slot: u64) -> SlotTracker {
        let mut tracker = SlotTracker::default();
        tracker.on_duties(vec![duty_response(slot)]);
        tracker
    }

    #[test]
    fn an_event_with_a_matching_duty_yields_a_context() {
        let mut tracker = tracker_with_duty(10);

        let context = tracker
            .on_payload_attributes(event(10, B256::repeat_byte(0x11)))
            .expect("a registered proposer and a complete event must build");

        assert_eq!(context.slot, 10);
        assert_eq!(context.parent_hash, B256::repeat_byte(0x11));
        assert_eq!(context.parent_block_number, 9);
        assert_eq!(context.timestamp, 1_700_000_000 + 120);
        assert_eq!(context.prev_randao, B256::repeat_byte(0xcc));
        assert_eq!(context.parent_beacon_block_root, B256::repeat_byte(0xdd));
        assert_eq!(context.proposer_pubkey, BlsPublicKeyBytes::from([7u8; 48]));
    }

    #[test]
    fn the_fee_recipient_and_gas_limit_come_from_the_duty() {
        let mut tracker = tracker_with_duty(10);

        let context = tracker.on_payload_attributes(event(10, B256::repeat_byte(0x11))).unwrap();

        // The event's `suggested_fee_recipient` is the local validator's. Paying
        // it would produce a block the relay rejects.
        assert_eq!(context.proposer_fee_recipient, PROPOSER_FEE_RECIPIENT);
        assert_eq!(context.registered_gas_limit, REGISTERED_GAS_LIMIT);
    }

    #[test]
    fn an_event_without_a_duty_is_skipped() {
        let mut tracker = SlotTracker::default();

        assert!(
            tracker.on_payload_attributes(event(10, B256::repeat_byte(0x11))).is_none(),
            "an unregistered proposer cannot be bid for"
        );
    }

    #[test]
    fn a_repeated_event_is_skipped() {
        let mut tracker = tracker_with_duty(10);
        let parent = B256::repeat_byte(0x11);

        assert!(tracker.on_payload_attributes(event(10, parent)).is_some());
        assert!(
            tracker.on_payload_attributes(event(10, parent)).is_none(),
            "the beacon node repeats an event whenever it recomputes the attributes"
        );
    }

    #[test]
    fn a_new_parent_for_the_same_slot_yields_a_fresh_context() {
        let mut tracker = tracker_with_duty(10);

        assert!(tracker.on_payload_attributes(event(10, B256::repeat_byte(0x11))).is_some());
        let reorged = tracker
            .on_payload_attributes(event(10, B256::repeat_byte(0x22)))
            .expect("a late or re-orged parent must rebuild, not count as a duplicate");

        assert_eq!(reorged.parent_hash, B256::repeat_byte(0x22));
    }

    #[test]
    fn an_event_for_an_older_slot_is_skipped() {
        let mut tracker = SlotTracker::default();
        tracker.on_duties(vec![duty_response(9), duty_response(10)]);

        assert!(tracker.on_payload_attributes(event(10, B256::repeat_byte(0x11))).is_some());
        assert!(
            tracker.on_payload_attributes(event(9, B256::repeat_byte(0x99))).is_none(),
            "an SSE reconnect can replay older events"
        );
    }

    #[test]
    fn an_event_without_a_parent_beacon_block_root_is_skipped() {
        let mut tracker = tracker_with_duty(10);
        let mut incomplete = event(10, B256::repeat_byte(0x11));
        incomplete.data.payload_attributes.parent_beacon_block_root = None;

        assert!(
            tracker.on_payload_attributes(incomplete).is_none(),
            "EIP-4788 makes the root mandatory"
        );
    }

    #[test]
    fn refreshed_duties_replace_the_previous_set() {
        let mut tracker = tracker_with_duty(10);
        assert!(tracker.duty(10).is_some());

        tracker.on_duties(vec![duty_response(11)]);

        assert!(tracker.duty(10).is_none(), "a duty dropped by the relay must not linger");
        assert!(tracker.duty(11).is_some());
    }

    #[test]
    fn duties_for_past_slots_are_pruned() {
        let mut tracker = SlotTracker::default();
        tracker.on_duties(vec![duty_response(10)]);
        tracker.on_payload_attributes(event(10, B256::repeat_byte(0x11)));

        tracker.on_duties(vec![duty_response(5), duty_response(20)]);

        assert!(tracker.duty(5).is_none(), "duties must not accumulate across epochs");
        assert!(tracker.duty(20).is_some());
    }

    #[test]
    fn parses_a_relay_duties_response() {
        let json = r#"[{
            "slot": "11111",
            "validator_index": "222",
            "entry": {
                "message": {
                    "fee_recipient": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "gas_limit": "36000000",
                    "timestamp": "1700000000",
                    "pubkey": "0x933ad9491b62059dd065b560d256d8957a8c402cc6e8d8ee7290ae11e8f7329267a8811c397529dac52ae1342ba58c95"
                },
                "signature": "0xa8e4f8ff4e9f2ea1f1a0f1e7f61b2a26eefbf2ae0cd2b3a2bcbb2c0b6a09ad8d3e33fdb0a1a5f8c99d1b0f4a9b04e6a20b9a2b6d43cd0f5cb61ecebba38a9d3e93a8b6c0e5b3d33da0bb2a9d0c9ee1b8b5f8f1f92d2ce7c4a6e6c4bb1a3f1c2d"
            },
            "preferences": {
                "censoring": false,
                "filtering": "global",
                "trusted_builders": null,
                "disable_optimistic": false
            }
        }]"#;

        let duties: Vec<BuilderGetValidatorsResponse> = serde_json::from_str(json).unwrap();
        let mut tracker = SlotTracker::default();
        tracker.on_duties(duties);

        let duty = tracker.duty(11111).expect("the quoted slot must round-trip");
        assert_eq!(duty.gas_limit, 36_000_000, "gas_limit is a quoted u64");
        assert_eq!(duty.fee_recipient, Address::repeat_byte(0xaa));
    }

    #[test]
    fn parses_a_payload_attributes_event() {
        let json = r#"{
            "version": "fulu",
            "data": {
                "proposer_index": "123",
                "proposal_slot": "11111",
                "parent_block_number": "999",
                "parent_block_root": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "parent_block_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
                "payload_attributes": {
                    "timestamp": "1700000000",
                    "prev_randao": "0x3333333333333333333333333333333333333333333333333333333333333333",
                    "suggested_fee_recipient": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "withdrawals": [{
                        "index": "1",
                        "validator_index": "2",
                        "address": "0xcccccccccccccccccccccccccccccccccccccccc",
                        "amount": "32000000000"
                    }],
                    "parent_beacon_block_root": "0x4444444444444444444444444444444444444444444444444444444444444444"
                }
            }
        }"#;

        let event: PayloadAttributesEvent = serde_json::from_str(json).unwrap();
        let mut tracker = SlotTracker::default();
        tracker.on_duties(vec![duty_response(11111)]);

        let context = tracker.on_payload_attributes(event).expect("a complete event must build");

        assert_eq!(context.slot, 11111);
        assert_eq!(context.parent_block_number, 999);
        assert_eq!(context.timestamp, 1_700_000_000);
        assert_eq!(context.parent_hash, B256::repeat_byte(0x22));
        assert_eq!(context.parent_beacon_block_root, B256::repeat_byte(0x44));
        assert_eq!(context.withdrawals.len(), 1);
        assert_eq!(context.withdrawals[0].amount, 32_000_000_000);
    }
}
