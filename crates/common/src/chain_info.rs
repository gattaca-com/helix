use std::time::Duration;

use alloy_primitives::B256;
use helix_types::{
    ChainSpec, EthSpec, ForkName, MAINNET_GENESIS_TIME, MainnetEthSpec, Slot, SlotClock,
    SlotClockTrait, duration_into_slot, new_slot_clock,
};

pub(crate) const MAINNET_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    75, 54, 61, 185, 78, 40, 97, 32, 215, 110, 185, 5, 52, 15, 221, 78, 84, 191, 233, 240, 107,
    243, 63, 246, 207, 90, 210, 127, 81, 27, 254, 149,
];

/// `ATTESTATION_DUE_BPS_GLOAS` from the Gloas preset: attestations are due a quarter
/// of the way into the slot.
const ATTESTATION_DUE_BPS_GLOAS: u32 = 2500;

/// Runtime config with all chain specific information
#[derive(Clone)]
pub struct ChainInfo {
    pub name: String,
    pub genesis_validators_root: B256,
    pub spec: ChainSpec,
    pub clock: SlotClock,
    pub genesis_time_in_secs: u64,
    pub builder_domain: B256,
    /// Domain for verifying Gloas builder-API `SignedBuilderRequestAuth` signatures. Not a
    /// consensus domain; see `ChainSpec::get_request_auth_domain`.
    pub request_auth_domain: B256,
}

impl ChainInfo {
    pub fn new(spec: ChainSpec, genesis_validators_root: B256, genesis_time_in_secs: u64) -> Self {
        let name = spec.config_name.clone().expect("spec config name should be set");
        let clock = new_slot_clock(genesis_time_in_secs, spec.get_slot_duration());
        let builder_domain = spec.get_builder_application_domain();
        let request_auth_domain = spec.get_request_auth_domain();
        Self {
            name,
            genesis_validators_root,
            spec,
            clock,
            genesis_time_in_secs,
            builder_domain,
            request_auth_domain,
        }
    }

    pub fn fork_at_slot(&self, slot: Slot) -> ForkName {
        self.spec.fork_name_at_slot::<MainnetEthSpec>(slot)
    }

    pub fn current_fork_name(&self) -> ForkName {
        self.fork_at_slot(self.current_slot())
    }

    pub fn seconds_per_slot(&self) -> u64 {
        self.spec.get_slot_duration().as_secs()
    }

    pub fn slots_per_epoch(&self) -> u64 {
        MainnetEthSpec::slots_per_epoch()
    }

    /// Returns the position of the slot in the epoch, 0-31.
    pub fn slot_in_epoch(&self, slot: Slot) -> u64 {
        slot.as_u64() % self.slots_per_epoch()
    }

    /// Duration since the start of the slot, None if we're before the start of the slot
    pub fn duration_into_slot(&self, slot: Slot) -> Option<Duration> {
        duration_into_slot(&self.clock, slot)
    }

    /// Current slot based on current clock time. Falls back to genesis slot (0) if we're
    /// running before the configured genesis time (e.g. a testnet whose genesis hasn't
    /// started yet) rather than panicking on every request.
    pub fn current_slot(&self) -> Slot {
        self.clock.now().unwrap_or(Slot::new(0))
    }

    /// Unix time at which `slot` starts.
    pub fn slot_start(&self, slot: Slot) -> Duration {
        Duration::from_secs(self.genesis_time_in_secs + slot.as_u64() * self.seconds_per_slot())
    }

    /// Gloas divides the slot in quarters: attestations are due at 25%, the payload
    /// reveal at 50%.
    pub fn gloas_attestation_deadline(&self) -> Duration {
        self.spec.get_slot_duration() * ATTESTATION_DUE_BPS_GLOAS / 10_000
    }

    /// How long to hold a Gloas payload before revealing it. Revealing before the
    /// attestation deadline lets an equivocating proposer build a competing block on
    /// the payload while the honest block still has no attestations behind it.
    pub fn gloas_reveal_delay(&self, slot: Slot, now: Duration) -> Duration {
        (self.slot_start(slot) + self.gloas_attestation_deadline()).saturating_sub(now)
    }

    pub fn max_blobs_per_block(&self) -> usize {
        let epoch = self.current_slot().epoch(self.slots_per_epoch());
        self.spec.max_blobs_per_block(epoch) as usize
    }
}

impl Default for ChainInfo {
    fn default() -> Self {
        let spec = ChainSpec::mainnet();
        Self::new(spec, MAINNET_GENESIS_VALIDATOR_ROOT.into(), MAINNET_GENESIS_TIME)
    }
}

#[cfg(test)]
mod gloas_reveal_tests {
    use super::*;

    fn chain_info() -> ChainInfo {
        ChainInfo::default()
    }

    #[test]
    fn the_deadline_is_a_quarter_of_the_slot() {
        let info = chain_info();
        assert_eq!(info.gloas_attestation_deadline(), Duration::from_secs(3));
    }

    #[test]
    fn a_reveal_asked_for_at_the_slot_start_waits_for_the_deadline() {
        let info = chain_info();
        let slot = Slot::new(1_000);
        let delay = info.gloas_reveal_delay(slot, info.slot_start(slot));
        assert_eq!(delay, Duration::from_secs(3));
    }

    #[test]
    fn a_reveal_asked_for_after_the_deadline_does_not_wait() {
        let info = chain_info();
        let slot = Slot::new(1_000);
        let now = info.slot_start(slot) + Duration::from_secs(5);
        assert_eq!(info.gloas_reveal_delay(slot, now), Duration::ZERO);
    }

    #[test]
    fn the_wait_never_runs_past_the_payload_deadline() {
        let info = chain_info();
        let slot = Slot::new(1_000);
        let delay = info.gloas_reveal_delay(slot, info.slot_start(slot));
        assert!(delay < info.spec.get_slot_duration() / 2);
    }
}
