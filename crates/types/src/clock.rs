use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use lh_slot_clock::{SlotClock as SlotClockTrait, SystemTimeSlotClock as SlotClock};

use crate::Slot;

pub const MAINNET_GENESIS_TIME: u64 = 1606824023;

pub fn new_slot_clock(genesis_time: u64, slot_duration: Duration) -> SlotClock {
    SlotClock::new(0u64.into(), Duration::from_secs(genesis_time), slot_duration)
}

pub fn duration_into_slot(clock: &SlotClock, slot: Slot) -> Option<Duration> {
    let slot_start = clock.start_of(slot)?;
    // safe since we're past UNIX_EPOCH
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().checked_sub(slot_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `duration_into_slot` takes its own live `SystemTime::now()` reading, so it can't be
    /// pinned to an exact expected value without mocking time. Instead this brackets it: the
    /// result must fall between `now - slot_start` measured just before and just after the
    /// call, which holds by construction regardless of scheduling delays — unlike comparing two
    /// independent live clock reads against a fixed tolerance, which is exactly what made the
    /// old version of this test flaky under parallel test-thread contention.
    #[test]
    fn test_duration_into_slot() {
        let clock = new_slot_clock(MAINNET_GENESIS_TIME, Duration::from_secs(12));
        let slot = clock.now().unwrap();
        let slot_start = clock.start_of(slot).unwrap();

        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap() - slot_start;
        let actual = duration_into_slot(&clock, slot).unwrap();
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap() - slot_start;

        assert!(
            actual >= before && actual <= after,
            "duration {actual:?} not within [{before:?}, {after:?}]"
        );
    }
}
