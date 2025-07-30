use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use lh_slot_clock::{SlotClock as SlotClockTrait, SystemTimeSlotClock as SlotClock};

use crate::Slot;

pub const MAINNET_GENESIS_TIME: u64 = 1606824023;
pub const SEPOLIA_GENESIS_TIME: u64 = 1655733600;
pub const HOLESKY_GENESIS_TIME: u64 = 1695902400;
pub const HOODI_GENESIS_TIME: u64 = 1742213400;

pub fn mainnet_slot_clock(seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(MAINNET_GENESIS_TIME),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn sepolia_slot_clock(seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(SEPOLIA_GENESIS_TIME),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn holesky_slot_clock(seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(HOLESKY_GENESIS_TIME),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn hoodi_slot_clock(seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(HOODI_GENESIS_TIME),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn custom_slot_clock(genesis_time: u64, seconds_per_slot: u64) -> SlotClock {
    SlotClock::new(
        0u64.into(),
        Duration::from_secs(genesis_time),
        Duration::from_secs(seconds_per_slot),
    )
}

pub fn duration_into_slot(clock: &SlotClock, slot: Slot) -> Option<Duration> {
    let slot_start = clock.start_of(slot)?;
    // safe since we're past UNIX_EPOCH
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().checked_sub(slot_start)
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use super::*;

    #[test]
    fn test_duration_into_slot() {
        let clock = mainnet_slot_clock(12);

        for _ in 0..100 {
            let slot = clock.now().unwrap();
            let dur_1 = clock.millis_from_current_slot_start().unwrap().as_nanos() as i128;
            let dur_2 = duration_into_slot(&clock, slot).unwrap().as_nanos() as i128;
            let delta = dur_1 - dur_2;
            assert!(delta.abs() < 1_000_000, "clock delta above 1ms: {}", delta);

            sleep(Duration::from_millis(10));
        }
    }
}
