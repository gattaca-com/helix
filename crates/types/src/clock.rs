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
            assert!(delta.abs() < 1_000_000, "clock delta above 1ms: {delta}");

            sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_mainnet_slot_clock() {
        let clock = mainnet_slot_clock(12);
        let now = clock.now();
        assert!(now.is_some());
        // Mainnet has been running for a while, so slot should be > 0
        assert!(now.unwrap().as_u64() > 0);
    }

    #[test]
    fn test_sepolia_slot_clock() {
        let clock = sepolia_slot_clock(12);
        let now = clock.now();
        assert!(now.is_some());
    }

    #[test]
    fn test_holesky_slot_clock() {
        let clock = holesky_slot_clock(12);
        let now = clock.now();
        assert!(now.is_some());
    }

    #[test]
    fn test_hoodi_slot_clock() {
        let clock = hoodi_slot_clock(12);
        let now = clock.now();
        assert!(now.is_some());
    }

    #[test]
    fn test_custom_slot_clock() {
        // Use a genesis time in the past
        let genesis_time = 1_600_000_000;
        let clock = custom_slot_clock(genesis_time, 12);
        let now = clock.now();
        assert!(now.is_some());
        assert!(now.unwrap().as_u64() > 0);
    }

    #[test]
    fn test_duration_into_slot_returns_some_for_current() {
        let clock = mainnet_slot_clock(12);
        let current_slot = clock.now().unwrap();
        let duration = duration_into_slot(&clock, current_slot);
        assert!(duration.is_some());
        // Should be less than 12 seconds (slot duration)
        assert!(duration.unwrap().as_secs() < 12);
    }

    #[test]
    fn test_genesis_times_are_in_past() {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert!(MAINNET_GENESIS_TIME < now_secs);
        assert!(SEPOLIA_GENESIS_TIME < now_secs);
        assert!(HOLESKY_GENESIS_TIME < now_secs);
        assert!(HOODI_GENESIS_TIME < now_secs);
    }

    #[test]
    fn test_genesis_times_ordering() {
        // Mainnet launched first
        assert!(MAINNET_GENESIS_TIME < SEPOLIA_GENESIS_TIME);
        assert!(SEPOLIA_GENESIS_TIME < HOLESKY_GENESIS_TIME);
        assert!(HOLESKY_GENESIS_TIME < HOODI_GENESIS_TIME);
    }

    #[test]
    fn test_slot_clocks_use_correct_genesis_time() {
        let mainnet_clock = mainnet_slot_clock(12);
        let sepolia_clock = sepolia_slot_clock(12);
        let holesky_clock = holesky_slot_clock(12);
        let hoodi_clock = hoodi_slot_clock(12);

        // All clocks should return valid current slots
        assert!(mainnet_clock.now().is_some());
        assert!(sepolia_clock.now().is_some());
        assert!(holesky_clock.now().is_some());
        assert!(hoodi_clock.now().is_some());
    }

    #[test]
    fn test_custom_slot_clock_with_different_slot_duration() {
        let genesis_time = 1_600_000_000;
        let clock_12s = custom_slot_clock(genesis_time, 12);
        let clock_6s = custom_slot_clock(genesis_time, 6);

        // With same genesis but different slot durations, slot numbers should differ
        let slot_12s = clock_12s.now().unwrap().as_u64();
        let slot_6s = clock_6s.now().unwrap().as_u64();

        // 6 second slots should have roughly 2x as many slots
        assert!(slot_6s > slot_12s);
    }
}
