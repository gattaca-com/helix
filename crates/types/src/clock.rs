use std::time::Duration;

pub use lh_slot_clock::SlotClock as SlotClockTrait;
pub use lh_slot_clock::SystemTimeSlotClock as SlotClock;

pub const MAINNET_GENESIS_TIME: u64 = 1606824023;
pub const SEPOLIA_GENESIS_TIME: u64 = 1655733600;
pub const HOLESKY_GENESIS_TIME: u64 = 1695902400;
pub const SECONDS_PER_SLOT: u64 = 12;
pub const SLOTS_PER_EPOCH: u64 = 32;

pub fn mainnet_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(MAINNET_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn sepolia_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(SEPOLIA_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn holesky_slot_clock() -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(HOLESKY_GENESIS_TIME),
        Duration::from_secs(SECONDS_PER_SLOT),
    )
}

pub fn custom_slot_clock(genesis_time: u64, seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(genesis_time),
        Duration::from_secs(seconds_per_slot),
    )
}
