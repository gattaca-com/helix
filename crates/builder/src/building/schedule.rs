use std::{collections::HashMap, future::Future, time::Duration};

use alloy_primitives::{B256, U256};
use tokio::sync::mpsc;

use crate::building::slot::SlotContext;

/// How long to wait before each build attempt, measured from `now_ms`.
///
/// Every entry is relative to the same instant, so a caller must sleep to an
/// absolute deadline rather than sleeping each in turn.
///
/// Empty once every offset has passed. Whether that still deserves an immediate
/// attempt is the caller's call, because it depends on whether the slot has been
/// bid on already.
pub fn delays(slot_timestamp: u64, offsets: &[u64], now_ms: u64) -> Vec<Duration> {
    let start_ms = slot_timestamp * 1_000;
    let mut sorted: Vec<u64> = offsets.to_vec();
    sorted.sort_unstable();

    sorted
        .iter()
        .filter_map(|offset| start_ms.checked_add(*offset)?.checked_sub(now_ms))
        .map(Duration::from_millis)
        .collect()
}

/// Runs one slot's attempts, one per delay, measured from a single instant.
async fn run_schedule<F, Fut>(slot: SlotContext, delays: Vec<Duration>, attempt: F)
where
    F: Fn(SlotContext) -> Fut,
    Fut: Future<Output = ()>,
{
    let base = tokio::time::Instant::now();
    for delay in delays {
        tokio::time::sleep_until(base + delay).await;
        attempt(slot.clone()).await;
    }
}

/// Drives one schedule at a time. A new context supersedes the one in flight:
/// the head moved, so every remaining attempt would bid on a parent the relay
/// has already replaced.
pub async fn drive<F, Fut, N>(
    mut contexts: mpsc::Receiver<SlotContext>,
    offsets: &[u64],
    now_ms: N,
    attempt: F,
) where
    F: Fn(SlotContext) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    N: Fn() -> u64,
{
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    let mut scheduled_slot: Option<u64> = None;

    while let Some(slot) = contexts.recv().await {
        let mut delays = delays(slot.timestamp, offsets, now_ms());
        if delays.is_empty() {
            if scheduled_slot == Some(slot.slot) {
                // A new head this late only replaces a bid already sent, and the
                // relay has moved on to the next bid slot by now, so an immediate
                // attempt is refused as a submission for the wrong slot.
                continue;
            }
            // Nothing has been sent for this slot, so one late attempt beats none.
            delays.push(Duration::ZERO);
        }

        if let Some(handle) = current.take() {
            handle.abort();
        }
        scheduled_slot = Some(slot.slot);
        current = Some(tokio::spawn(run_schedule(slot, delays, attempt.clone())));
    }

    if let Some(handle) = current.take() {
        let _ = handle.await;
    }
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
    use std::sync::{Arc, Mutex};

    use helix_types::{BlsPublicKeyBytes, Withdrawals};

    use super::*;

    fn context(slot: u64, parent: u8, timestamp: u64) -> SlotContext {
        SlotContext {
            slot,
            parent_hash: B256::repeat_byte(parent),
            parent_block_number: Some(slot.saturating_sub(1)),
            timestamp,
            prev_randao: B256::ZERO,
            withdrawals: Withdrawals::default(),
            parent_beacon_block_root: B256::ZERO,
            proposer_pubkey: BlsPublicKeyBytes::default(),
            proposer_fee_recipient: alloy_primitives::Address::ZERO,
            registered_gas_limit: 30_000_000,
        }
    }

    /// Drives `contexts` and reports the parent of every attempt, in order.
    async fn attempts_for(contexts: Vec<SlotContext>, offsets: &[u64], now_ms: u64) -> Vec<B256> {
        let (tx, rx) = mpsc::channel(16);
        for context in contexts {
            tx.send(context).await.unwrap();
        }
        drop(tx);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        drive(
            rx,
            offsets,
            move || now_ms,
            move |slot: SlotContext| {
                let recorder = recorder.clone();
                async move {
                    recorder.lock().unwrap().push(slot.parent_hash);
                }
            },
        )
        .await;

        let attempts = seen.lock().unwrap().clone();
        attempts
    }

    /// The head moved, so the first context's remaining attempts would bid on a
    /// parent the relay has already replaced. Before the driver they ran anyway,
    /// late and all at once, and the relay answered "unknown parent hash" or
    /// "submission for wrong slot".
    #[tokio::test]
    async fn a_superseded_context_never_attempts() {
        let attempts = attempts_for(
            vec![context(1, 0xa1, SLOT_TIMESTAMP), context(1, 0xb2, SLOT_TIMESTAMP)],
            &[20, 40],
            START_MS,
        )
        .await;

        assert_eq!(
            attempts,
            vec![B256::repeat_byte(0xb2), B256::repeat_byte(0xb2)],
            "only the newest parent may be bid on",
        );
    }

    /// The fallback in `delays` still has to hold: a slot learned about after
    /// every offset has passed gets one attempt rather than none.
    /// The head moved again after the last offset had passed. The relay has moved
    /// on to the next bid slot by then, so submitting produced a steady stream of
    /// "submission for wrong slot" -- and a bid was already sent for this slot
    /// anyway.
    #[tokio::test]
    async fn a_late_replacement_does_not_attempt() {
        let attempts = attempts_for(
            vec![
                context(1, 0xa1, SLOT_TIMESTAMP),
                // Arrives once both offsets are long past.
                context(1, 0xb2, SLOT_TIMESTAMP),
            ],
            &[20, 40],
            START_MS + 9_000,
        )
        .await;

        assert_eq!(
            attempts,
            vec![B256::repeat_byte(0xa1)],
            "the first context still earns its one late attempt; the replacement adds nothing",
        );
    }

    #[tokio::test]
    async fn a_slot_learned_about_late_still_attempts_once() {
        let attempts =
            attempts_for(vec![context(1, 0xa1, SLOT_TIMESTAMP)], &[20, 40], START_MS + 9_000).await;

        assert_eq!(attempts, vec![B256::repeat_byte(0xa1)]);
    }

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

    /// Whether a passed slot still deserves an attempt depends on whether anything
    /// has been sent for it, which only the driver knows -- see
    /// `a_slot_learned_about_late_still_attempts_once` and
    /// `a_late_replacement_does_not_attempt`.
    #[test]
    fn every_offset_past_leaves_no_delays() {
        let delays = delays(SLOT_TIMESTAMP, &[500, 2000], START_MS + 5_000);

        assert!(delays.is_empty());
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
