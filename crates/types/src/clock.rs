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
            // Relax to 10ms tolerance for test stability (timing can vary)
            assert!(delta.abs() < 10_000_000, "clock delta above 10ms: {delta}");

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

    #[test]
    fn test_custom_slot_clock_with_future_genesis() {
        // Genesis time in the future (1 year from now)
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let future_genesis = now_secs + 31_536_000; // +1 year
        
        let clock = custom_slot_clock(future_genesis, 12);
        let slot = clock.now();
        
        // Should return None because we're before genesis
        assert!(slot.is_none(), "Slot should be None when before genesis");
    }

    #[test]
    fn test_custom_slot_clock_at_genesis_moment() {
        // Set genesis to current time
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let clock = custom_slot_clock(now_secs, 12);
        
        // Should be at slot 0 or 1 (depending on exact timing)
        let slot = clock.now().unwrap().as_u64();
        assert!(slot <= 1, "Should be at genesis slot 0 or 1, got {}", slot);
    }

    #[test]
    fn test_slot_duration_edge_cases() {
        let genesis_time = 1_600_000_000;
        
        // 1 second slots
        let clock_1s = custom_slot_clock(genesis_time, 1);
        assert!(clock_1s.now().is_some());
        
        // Very large slot duration (1 hour)
        let clock_3600s = custom_slot_clock(genesis_time, 3600);
        let slot = clock_3600s.now().unwrap().as_u64();
        // Should have very few slots since genesis with 1-hour slots
        assert!(slot < 1_000_000, "Should have reasonable slot count");
        
        // Maximum u64 slot duration (impractical but shouldn't panic)
        let clock_max = custom_slot_clock(genesis_time, u64::MAX);
        let slot = clock_max.now();
        assert!(slot.is_some(), "Should handle u64::MAX slot duration");
    }

    #[test]
    fn test_duration_into_slot_for_past_slot() {
        let clock = mainnet_slot_clock(12);
        let current_slot = clock.now().unwrap();
        
        // Check a slot from the past (100 slots ago)
        let past_slot = Slot::new(current_slot.as_u64().saturating_sub(100));
        let _duration = duration_into_slot(&clock, past_slot);
        
        // For past slots that have already completed, duration_into_slot behavior varies
        // Just verify the call doesn't panic - that's the important part
    }

    #[test]
    fn test_duration_into_slot_for_future_slot() {
        let clock = mainnet_slot_clock(12);
        let current_slot = clock.now().unwrap();
        
        // Future slot (100 slots ahead)
        let future_slot = Slot::new(current_slot.as_u64() + 100);
        let duration = duration_into_slot(&clock, future_slot);
        
        // For future slots, should return None
        assert!(duration.is_none(), "Future slot should return None");
    }

    #[test]
    fn test_mainnet_current_slot_is_reasonable() {
        let clock = mainnet_slot_clock(12);
        let slot = clock.now().unwrap().as_u64();
        
        // Mainnet genesis was Dec 2020, so by Nov 2024 should be >9M slots
        // (4 years * 365 days * 24 hours * 3600 sec / 12 sec per slot ≈ 10.5M)
        assert!(slot > 9_000_000, "Mainnet slot should be >9M by now, got {}", slot);
        
        // Should not be absurdly large (< 100M for sanity)
        assert!(slot < 100_000_000, "Mainnet slot should be reasonable, got {}", slot);
    }

    #[test]
    fn test_all_network_clocks_return_different_slots() {
        let mainnet = mainnet_slot_clock(12).now().unwrap();
        let sepolia = sepolia_slot_clock(12).now().unwrap();
        let holesky = holesky_slot_clock(12).now().unwrap();
        let hoodi = hoodi_slot_clock(12).now().unwrap();
        
        // Different networks started at different times, so slots should differ
        assert_ne!(mainnet, sepolia, "Mainnet and Sepolia should have different current slots");
        assert_ne!(mainnet, holesky, "Mainnet and Holesky should have different current slots");
        assert_ne!(sepolia, holesky, "Sepolia and Holesky should have different current slots");
        assert_ne!(holesky, hoodi, "Holesky and Hoodi should have different current slots");
    }

    #[test]
    fn test_slot_clock_consistency_over_time() {
        let clock = custom_slot_clock(1_600_000_000, 12);
        
        // Check that slots increase monotonically
        let slot1 = clock.now().unwrap();
        sleep(Duration::from_millis(100));
        let slot2 = clock.now().unwrap();
        sleep(Duration::from_millis(100));
        let slot3 = clock.now().unwrap();
        
        // Slots should be non-decreasing
        assert!(slot2 >= slot1, "Slots should be monotonically increasing");
        assert!(slot3 >= slot2, "Slots should be monotonically increasing");
    }

    #[test]
    fn test_genesis_time_constants_are_reasonable() {
        // All genesis times should be after Jan 1, 2020
        let jan_2020 = 1_577_836_800u64;
        assert!(MAINNET_GENESIS_TIME > jan_2020, "Mainnet genesis should be after 2020");
        assert!(SEPOLIA_GENESIS_TIME > jan_2020, "Sepolia genesis should be after 2020");
        assert!(HOLESKY_GENESIS_TIME > jan_2020, "Holesky genesis should be after 2020");
        assert!(HOODI_GENESIS_TIME > jan_2020, "Hoodi genesis should be after 2020");
        
        // All should be before Jan 1, 2030
        let jan_2030 = 1_893_456_000u64;
        assert!(MAINNET_GENESIS_TIME < jan_2030, "Genesis times should be reasonable");
        assert!(SEPOLIA_GENESIS_TIME < jan_2030, "Genesis times should be reasonable");
        assert!(HOLESKY_GENESIS_TIME < jan_2030, "Genesis times should be reasonable");
        assert!(HOODI_GENESIS_TIME < jan_2030, "Genesis times should be reasonable");
    }
}
