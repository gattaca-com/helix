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

/// Runtime config with all chain specific information
#[derive(Clone)]
pub struct ChainInfo {
    pub name: String,
    pub genesis_validators_root: B256,
    pub spec: ChainSpec,
    pub clock: SlotClock,
    pub genesis_time_in_secs: u64,
    pub builder_domain: B256,
}

impl ChainInfo {
    pub fn new(spec: ChainSpec, genesis_validators_root: B256, genesis_time_in_secs: u64) -> Self {
        let name = spec.config_name.clone().expect("spec config name should be set");
        let clock = new_slot_clock(genesis_time_in_secs, spec.seconds_per_slot);
        let builder_domain = spec.get_builder_domain();
        Self { name, genesis_validators_root, spec, clock, genesis_time_in_secs, builder_domain }
    }

    pub fn fork_at_slot(&self, slot: Slot) -> ForkName {
        self.spec.fork_name_at_slot::<MainnetEthSpec>(slot)
    }

    pub fn current_fork_name(&self) -> ForkName {
        let current_slot = self.clock.now().unwrap();
        self.fork_at_slot(current_slot)
    }

    pub fn seconds_per_slot(&self) -> u64 {
        self.spec.seconds_per_slot
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

    /// Current slot based on current clock time
    pub fn current_slot(&self) -> Slot {
        // safe since we're past genesis slot and UNIX_EPOCH
        self.clock.now().unwrap()
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
