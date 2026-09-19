use std::{collections::HashMap, future::Future, time::Duration};

use alloy_primitives::{B256, U256};
use tokio::sync::mpsc;

use crate::building::slot::SlotContext;

/// A floor between attempts, so a build that fails immediately cannot spin.
const MIN_ATTEMPT_INTERVAL: Duration = Duration::from_millis(25);

/// What the relay's answer says about whether this slot is still worth bidding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// Keep going: a better block may still win the slot.
    Continue,
    /// The relay has moved on to the next bid slot, so nothing further is
    /// accepted and building again would only waste the CPU.
    SlotClosed,
}

/// When to start building for a slot: `lead_ms` before it begins, or now if that
/// moment has passed.
fn start_delay(slot_timestamp: u64, lead_ms: u64, now_ms: u64) -> Duration {
    let start_ms = (slot_timestamp * 1_000).saturating_sub(lead_ms);
    Duration::from_millis(start_ms.saturating_sub(now_ms))
}

/// Builds and submits without pause from `lead_ms` before the slot starts, each
/// attempt picking up whatever the mempool has gained, until the relay says the
/// slot has closed or the slot itself has run out.
async fn run_schedule<F, Fut>(slot: SlotContext, start: Duration, budget: Duration, attempt: F)
where
    F: Fn(SlotContext) -> Fut,
    Fut: Future<Output = Attempt>,
{
    let base = tokio::time::Instant::now();
    tokio::time::sleep_until(base + start).await;

    let deadline = base + start + budget;
    while tokio::time::Instant::now() < deadline {
        let attempted_at = tokio::time::Instant::now();
        if attempt(slot.clone()).await == Attempt::SlotClosed {
            return;
        }
        tokio::time::sleep_until(attempted_at + MIN_ATTEMPT_INTERVAL).await;
    }
}

/// Drives one slot at a time. A new context supersedes the one in flight: the
/// head moved, so every further attempt would bid on a parent the relay has
/// already replaced.
pub async fn drive<F, Fut, N>(
    mut contexts: mpsc::Receiver<SlotContext>,
    lead_ms: u64,
    budget: Duration,
    now_ms: N,
    attempt: F,
) where
    F: Fn(SlotContext) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Attempt> + Send + 'static,
    N: Fn() -> u64,
{
    let mut current: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(slot) = contexts.recv().await {
        if let Some(handle) = current.take() {
            handle.abort();
        }
        let start = start_delay(slot.timestamp, lead_ms, now_ms());
        current = Some(tokio::spawn(run_schedule(slot, start, budget, attempt.clone())));
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use helix_types::{BlsPublicKeyBytes, Withdrawals};

    use super::*;

    const SLOT_TIMESTAMP: u64 = 1_700_000_000;
    const START_MS: u64 = SLOT_TIMESTAMP * 1_000;
    const LEAD_MS: u64 = 2_000;
    const BUDGET: Duration = Duration::from_millis(400);

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

    /// Drives `contexts`, answering `outcome` for each attempt, and reports the
    /// parent of every attempt in order.
    async fn attempts_for<F>(contexts: Vec<SlotContext>, now_ms: u64, outcome: F) -> Vec<B256>
    where
        F: Fn(usize) -> Attempt + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(16);
        for context in contexts {
            tx.send(context).await.unwrap();
        }
        drop(tx);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let recorder = seen.clone();
        let outcome = Arc::new(outcome);
        drive(
            rx,
            LEAD_MS,
            BUDGET,
            move || now_ms,
            move |slot: SlotContext| {
                let (recorder, calls, outcome) = (recorder.clone(), calls.clone(), outcome.clone());
                async move {
                    recorder.lock().unwrap().push(slot.parent_hash);
                    outcome(calls.fetch_add(1, Ordering::SeqCst))
                }
            },
        )
        .await;

        let attempts = seen.lock().unwrap().clone();
        attempts
    }

    #[test]
    fn building_starts_before_the_slot_does() {
        let delay = start_delay(SLOT_TIMESTAMP, LEAD_MS, START_MS - 10_000);

        assert_eq!(delay, Duration::from_millis(8_000), "2s before a slot 10s away");
    }

    #[test]
    fn a_slot_learned_about_late_starts_at_once() {
        let delay = start_delay(SLOT_TIMESTAMP, LEAD_MS, START_MS + 500);

        assert_eq!(delay, Duration::ZERO, "no waiting for a moment already gone");
    }

    /// The point of the loop: keep building better blocks for as long as the
    /// relay will take them, rather than bidding once and stopping.
    #[tokio::test]
    async fn it_keeps_building_until_the_relay_closes_the_slot() {
        let attempts = attempts_for(vec![context(1, 0xa1, SLOT_TIMESTAMP)], START_MS, |n| {
            if n >= 3 { Attempt::SlotClosed } else { Attempt::Continue }
        })
        .await;

        assert_eq!(attempts.len(), 4, "three accepted attempts, then the closing one");
        assert!(attempts.iter().all(|p| *p == B256::repeat_byte(0xa1)));
    }

    /// Once the relay is bidding for the next slot nothing more can be accepted,
    /// so further building is wasted.
    #[tokio::test]
    async fn it_stops_as_soon_as_the_slot_closes() {
        let attempts =
            attempts_for(vec![context(1, 0xa1, SLOT_TIMESTAMP)], START_MS, |_| Attempt::SlotClosed)
                .await;

        assert_eq!(attempts.len(), 1);
    }

    /// The head moved, so everything still to come would bid on a parent the
    /// relay has already replaced.
    #[tokio::test]
    async fn a_superseded_context_stops_attempting() {
        let attempts = attempts_for(
            vec![context(1, 0xa1, SLOT_TIMESTAMP), context(1, 0xb2, SLOT_TIMESTAMP)],
            START_MS,
            |n| if n >= 2 { Attempt::SlotClosed } else { Attempt::Continue },
        )
        .await;

        assert!(
            attempts.iter().all(|p| *p == B256::repeat_byte(0xb2)),
            "only the newest parent may be bid on, got: {attempts:?}",
        );
    }

    /// Without a closing answer the loop still has to end, or a dead relay would
    /// leave it building for ever.
    #[tokio::test]
    async fn the_budget_ends_a_slot_the_relay_never_closes() {
        let attempts =
            attempts_for(vec![context(1, 0xa1, SLOT_TIMESTAMP)], START_MS, |_| Attempt::Continue)
                .await;

        assert!(!attempts.is_empty(), "it must try");
        assert!(attempts.len() < 100, "the budget must stop it, got {} attempts", attempts.len(),);
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
