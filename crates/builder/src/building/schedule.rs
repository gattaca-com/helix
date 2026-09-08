use std::{collections::HashMap, time::Duration};

use alloy_primitives::{B256, U256};

/// How long to wait before each build attempt, measured from `now_ms`.
///
/// Every entry is relative to the same instant, so a caller must sleep to an
/// absolute deadline rather than sleeping each in turn.
///
/// A slot learned about late still gets one immediate attempt: dropping it
/// would mean no bid at all for that slot.
pub fn delays(slot_timestamp: u64, offsets: &[u64], now_ms: u64) -> Vec<Duration> {
    let start_ms = slot_timestamp * 1_000;
    let mut sorted: Vec<u64> = offsets.to_vec();
    sorted.sort_unstable();

    let upcoming: Vec<Duration> = sorted
        .iter()
        .filter_map(|offset| start_ms.checked_add(*offset)?.checked_sub(now_ms))
        .map(Duration::from_millis)
        .collect();

    if upcoming.is_empty() { vec![Duration::ZERO] } else { upcoming }
}

/// The best bid already sent, per slot and parent.
///
/// The relay treats every submission as a new bid, so resending a lower value
/// would replace a better one.
#[derive(Debug, Default)]
pub struct BestBid {
    best: HashMap<(u64, B256), U256>,
}

impl BestBid {
    /// Records `value` and reports whether it is worth submitting.
    pub fn improves(&mut self, slot: u64, parent: B256, value: U256) -> bool {
        match self.best.entry((slot, parent)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if value <= *entry.get() {
                    return false;
                }
                entry.insert(value);
                true
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(value);
                true
            }
        }
    }

    /// Drops slots below `slot`, so the map does not grow without bound.
    pub fn prune(&mut self, slot: u64) {
        self.best.retain(|(best_slot, _), _| *best_slot >= slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT_TIMESTAMP: u64 = 1_700_000_000;
    const START_MS: u64 = SLOT_TIMESTAMP * 1_000;

    #[test]
    fn offsets_become_delays_from_the_slot_start() {
        let delays = delays(SLOT_TIMESTAMP, &[500, 2000], START_MS);

        assert_eq!(delays, vec![Duration::from_millis(500), Duration::from_millis(2000)]);
    }

    #[test]
    fn unsorted_offsets_are_ordered() {
        let delays = delays(SLOT_TIMESTAMP, &[2000, 500], START_MS);

        assert_eq!(
            delays,
            vec![Duration::from_millis(500), Duration::from_millis(2000)],
            "the config is a plain list and nothing else sorts it",
        );
    }

    #[test]
    fn an_offset_already_past_is_skipped() {
        let delays = delays(SLOT_TIMESTAMP, &[500, 2000], START_MS + 1_000);

        assert_eq!(delays, vec![Duration::from_millis(1000)], "only the 2000ms offset is ahead");
    }

    #[test]
    fn a_late_event_still_gets_one_attempt() {
        let delays = delays(SLOT_TIMESTAMP, &[500, 2000], START_MS + 5_000);

        assert_eq!(
            delays,
            vec![Duration::ZERO],
            "a late payload_attributes must not mean no bid for the slot",
        );
    }

    #[test]
    fn a_higher_value_is_submitted() {
        let mut best = BestBid::default();
        let parent = B256::repeat_byte(0x11);

        assert!(best.improves(1, parent, U256::from(10)));
        assert!(best.improves(1, parent, U256::from(11)));
    }

    #[test]
    fn an_equal_or_lower_value_is_not_resubmitted() {
        let mut best = BestBid::default();
        let parent = B256::repeat_byte(0x11);
        assert!(best.improves(1, parent, U256::from(10)));

        assert!(!best.improves(1, parent, U256::from(10)), "an equal bid replaces a good one");
        assert!(!best.improves(1, parent, U256::from(9)));
    }

    #[test]
    fn a_new_slot_resets_the_best_value() {
        let mut best = BestBid::default();
        let parent = B256::repeat_byte(0x11);
        assert!(best.improves(1, parent, U256::from(10)));

        assert!(best.improves(2, parent, U256::from(1)), "a new slot starts a new auction");
    }

    #[test]
    fn a_new_parent_for_the_same_slot_resets_the_best_value() {
        let mut best = BestBid::default();
        assert!(best.improves(1, B256::repeat_byte(0x11), U256::from(10)));

        assert!(
            best.improves(1, B256::repeat_byte(0x22), U256::from(1)),
            "after a re-org the earlier bid sits on a dead parent",
        );
    }
}
