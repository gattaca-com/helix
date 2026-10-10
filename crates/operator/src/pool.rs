//! Shared collateral pool state. Phase 1 of the v4 optimistic spec: retain and replay protocol
//! state and compute available collateral. Pure; no I/O.
//!
//! A pool is identified by `collateral_id` (equal to `builder_info.builder_id`). The binding
//! `pubkey -> collateral_id` is write-once and permanent; member sets are append-only, so a
//! pool's members are exactly the pubkeys bound to it.

use alloy_primitives::U256;
use helix_common::PromotionMode;
use helix_types::{
    BlsPublicKeyBytes, BuilderCollateral, CollateralMembership, Demotion, OperatorMessage,
    Promotion,
};
use rustc_hash::FxHashMap;

/// Retained reports per pool. A demotion is a rare, serious event; this only bounds a faulty peer.
const MAX_REPORTS_PER_POOL: usize = 1024;
/// Pools held with no membership. Reports for these are retained but inert, so they cannot be
/// dropped without making state depend on arrival order.
const MAX_UNBOUND_POOLS: usize = 256;

type CollateralId = Vec<u8>;
type OperatorGroup = Vec<u8>;
/// Report identity. The pool is the outer key, so it is not repeated here.
type ReportKey = (BlsPublicKeyBytes, u64, alloy_primitives::B256);

#[derive(Debug)]
pub(crate) enum PoolError {
    /// A pubkey may never change pool.
    BindingConflict { pubkey: BlsPublicKeyBytes, existing: CollateralId, proposed: CollateralId },
    /// A newer member set omitted a bound pubkey.
    MembershipRemoval { collateral_id: CollateralId, missing: BlsPublicKeyBytes },
    /// Bound exceeded; the message is dropped.
    Capacity,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindingConflict { pubkey, existing, proposed } => write!(
                f,
                "pubkey {pubkey} is bound to pool {} and may not move to {}",
                String::from_utf8_lossy(existing),
                String::from_utf8_lossy(proposed),
            ),
            Self::MembershipRemoval { collateral_id, missing } => write!(
                f,
                "member set for pool {} omits bound pubkey {missing}",
                String::from_utf8_lossy(collateral_id),
            ),
            Self::Capacity => f.write_str("pool state at capacity"),
        }
    }
}

#[derive(Default)]
struct Pool {
    /// Dense index, assigned on first sight and never reused. Keys the derived view in
    /// `LocalCache` so the submission path hashes four bytes rather than a byte string, with no
    /// collision risk.
    index: u32,
    members: Vec<BlsPublicKeyBytes>,
    /// Timestamp of the newest accepted member set. Distinguishes a stale set from a current one.
    seen_ts: u64,
    /// Gross backing by operator group, before reservations, with the timestamp that set it.
    collateral: FxHashMap<OperatorGroup, (u128, u64)>,
    promotion: Option<Promotion>,
    reports: FxHashMap<ReportKey, Demotion>,
    /// Maximum offending bid value per slot. Maintained with `reports`.
    slot_max: FxHashMap<u64, u128>,
    /// Sum of `slot_max`. A sum of u128 maxima can exceed u128.
    reserved: U256,
}

impl Pool {
    /// True when the report is not superseded by the pool's promotion. A report wins a tie.
    fn retains(&self, ts_ms: u64) -> bool {
        self.promotion.as_ref().is_none_or(|p| ts_ms >= p.ts_ms)
    }

    fn insert_report(&mut self, demotion: Demotion) -> bool {
        let key = (demotion.builder_pubkey, demotion.slot, demotion.block_hash);
        match self.reports.get_mut(&key) {
            // Same report. Keep the larger value so the result does not depend on arrival order.
            Some(existing) if demotion.bid_value_wei <= existing.bid_value_wei => return false,
            Some(existing) => *existing = demotion,
            None => {
                self.reports.insert(key, demotion);
            }
        }
        self.recompute_reservations();
        true
    }

    fn recompute_reservations(&mut self) {
        self.slot_max.clear();
        for report in self.reports.values() {
            let slot_max = self.slot_max.entry(report.slot).or_default();
            *slot_max = (*slot_max).max(report.bid_value_wei);
        }
        self.reserved = self.slot_max.values().map(|wei| U256::from(*wei)).sum();
    }
}

pub(crate) struct PoolObservation<'a> {
    pub collateral_id: &'a [u8],
    pub gross: U256,
    pub reserved: U256,
    pub available: U256,
    pub reports: usize,
    pub members: usize,
    pub optimistic: usize,
}

pub(crate) struct PoolState {
    /// Operator group of this instance. Its own contribution is the gross backing below `Share`.
    local_group: OperatorGroup,
    mode: PromotionMode,
    binding: FxHashMap<BlsPublicKeyBytes, CollateralId>,
    pools: FxHashMap<CollateralId, Pool>,
}

impl PoolState {
    pub(crate) fn new(local_group: OperatorGroup, mode: PromotionMode) -> Self {
        Self { local_group, mode, binding: FxHashMap::default(), pools: FxHashMap::default() }
    }

    /// Pools are never removed, so the map length is the next free index.
    fn pool_mut(&mut self, collateral_id: &[u8]) -> &mut Pool {
        let index = self.pools.len() as u32;
        self.pools
            .entry(collateral_id.to_vec())
            .or_insert_with(|| Pool { index, ..Pool::default() })
    }

    /// Record `pubkey -> collateral_id`, rejecting a move to a different pool.
    fn bind(&mut self, pubkey: BlsPublicKeyBytes, collateral_id: &[u8]) -> Result<bool, PoolError> {
        match self.binding.get(&pubkey) {
            Some(existing) if existing == collateral_id => return Ok(false),
            Some(existing) => {
                return Err(PoolError::BindingConflict {
                    pubkey,
                    existing: existing.clone(),
                    proposed: collateral_id.to_vec(),
                });
            }
            None => {}
        }
        self.binding.insert(pubkey, collateral_id.to_vec());
        self.pool_mut(collateral_id).members.push(pubkey);
        Ok(true)
    }

    /// Member sets are append-only, so this is a union. Rejected whole on conflict: the message is
    /// a complete set, and two operators accepting different subsets would diverge undetected.
    pub(crate) fn apply_membership(
        &mut self,
        membership: &CollateralMembership,
    ) -> Result<bool, PoolError> {
        let pool = self.pool_mut(&membership.collateral_id);
        let newer = membership.ts_ms > pool.seen_ts;

        // Validate the whole set before mutating anything.
        for pubkey in &membership.builder_pubkeys {
            if let Some(existing) = self.binding.get(pubkey) &&
                existing != &membership.collateral_id
            {
                return Err(PoolError::BindingConflict {
                    pubkey: *pubkey,
                    existing: existing.clone(),
                    proposed: membership.collateral_id.clone(),
                });
            }
        }
        // A stale set legitimately omits pubkeys added after it was published.
        if newer &&
            let Some(missing) = self.pools[&membership.collateral_id]
                .members
                .iter()
                .find(|bound| !membership.builder_pubkeys.contains(bound))
        {
            return Err(PoolError::MembershipRemoval {
                collateral_id: membership.collateral_id.clone(),
                missing: *missing,
            });
        }

        let mut changed = false;
        for pubkey in &membership.builder_pubkeys {
            changed |= self.bind(*pubkey, &membership.collateral_id)?;
        }
        if newer {
            self.pools.get_mut(&membership.collateral_id).expect("inserted above").seen_ts =
                membership.ts_ms;
        }
        Ok(changed)
    }

    /// `group` is resolved by the caller from the message or the source peer.
    pub(crate) fn apply_collateral(
        &mut self,
        group: OperatorGroup,
        collateral: &BuilderCollateral,
    ) -> Result<bool, PoolError> {
        if self.pools.len() >= MAX_UNBOUND_POOLS &&
            !self.pools.contains_key(&collateral.collateral_id)
        {
            return Err(PoolError::Capacity);
        }
        let pool = self.pool_mut(&collateral.collateral_id);
        match pool.collateral.get(&group) {
            Some((_, ts_ms)) if collateral.ts_ms <= *ts_ms => Ok(false),
            _ => {
                pool.collateral.insert(group, (collateral.collateral_wei, collateral.ts_ms));
                Ok(true)
            }
        }
    }

    /// Retain the newest promotion and discard reports it supersedes.
    pub(crate) fn apply_promotion(&mut self, promotion: &Promotion) -> Result<bool, PoolError> {
        if let Some(existing) = self.binding.get(&promotion.builder_pubkey) &&
            existing != &promotion.collateral_id
        {
            return Err(PoolError::BindingConflict {
                pubkey: promotion.builder_pubkey,
                existing: existing.clone(),
                proposed: promotion.collateral_id.clone(),
            });
        }
        let pool = self.pool_mut(&promotion.collateral_id);
        if pool.promotion.as_ref().is_some_and(|p| promotion.ts_ms <= p.ts_ms) {
            return Ok(false);
        }
        pool.promotion = Some(promotion.clone());
        pool.reports.retain(|_, report| report.ts_ms >= promotion.ts_ms);
        pool.recompute_reservations();
        Ok(true)
    }

    /// A report for an unknown or unbacked pool is retained and inert. Dropping it would make
    /// state depend on arrival order.
    pub(crate) fn apply_demotion(&mut self, demotion: &Demotion) -> Result<bool, PoolError> {
        if let Some(existing) = self.binding.get(&demotion.builder_pubkey) &&
            existing != &demotion.collateral_id
        {
            return Err(PoolError::BindingConflict {
                pubkey: demotion.builder_pubkey,
                existing: existing.clone(),
                proposed: demotion.collateral_id.clone(),
            });
        }
        if self.pools.len() >= MAX_UNBOUND_POOLS &&
            !self.pools.contains_key(&demotion.collateral_id)
        {
            return Err(PoolError::Capacity);
        }
        let pool = self.pool_mut(&demotion.collateral_id);
        if !pool.retains(demotion.ts_ms) {
            return Ok(false);
        }
        if pool.reports.len() >= MAX_REPORTS_PER_POOL {
            return Err(PoolError::Capacity);
        }
        Ok(pool.insert_report(demotion.clone()))
    }

    /// Gross backing before reservations: this operator group's contribution, or one contribution
    /// per group under `Share`. Contributions from one group are never summed.
    pub(crate) fn gross(&self, collateral_id: &[u8]) -> U256 {
        let Some(pool) = self.pools.get(collateral_id) else {
            return U256::ZERO;
        };
        match self.mode {
            PromotionMode::Share => pool.collateral.values().map(|(wei, _)| U256::from(*wei)).sum(),
            PromotionMode::Observe | PromotionMode::Follow => pool
                .collateral
                .get(&self.local_group)
                .map_or(U256::ZERO, |(wei, _)| U256::from(*wei)),
        }
    }

    pub(crate) fn reserved(&self, collateral_id: &[u8]) -> U256 {
        self.pools.get(collateral_id).map_or(U256::ZERO, |pool| pool.reserved)
    }

    pub(crate) fn available(&self, collateral_id: &[u8]) -> U256 {
        self.gross(collateral_id).saturating_sub(self.reserved(collateral_id))
    }

    /// Optimistic requires a known binding, a pool promotion, and no retained report.
    pub(crate) fn is_optimistic(&self, pubkey: &BlsPublicKeyBytes) -> bool {
        let Some(collateral_id) = self.binding.get(pubkey) else {
            return false;
        };
        let Some(pool) = self.pools.get(collateral_id) else {
            return false;
        };
        pool.promotion.is_some() && !pool.reports.keys().any(|(reported, _, _)| reported == pubkey)
    }

    /// Pool half of the derived view: the budget every member shares. One record per pool.
    pub(crate) fn pool_records(&self) -> impl Iterator<Item = (u32, Vec<u8>, U256, bool)> {
        self.pools.iter().map(|(collateral_id, pool)| {
            (
                pool.index,
                collateral_id.clone(),
                self.available(collateral_id),
                pool.promotion.is_some(),
            )
        })
    }

    /// Per-pubkey half: which pool, and whether this key has a retained report.
    pub(crate) fn key_records(&self) -> impl Iterator<Item = (BlsPublicKeyBytes, u32, bool)> {
        self.pools.values().flat_map(|pool| {
            pool.members.iter().map(move |pubkey| {
                (*pubkey, pool.index, pool.reports.keys().any(|(key, _, _)| key == pubkey))
            })
        })
    }

    /// Phase 1 output. Recorded, not applied to admission.
    pub(crate) fn observe(&self) -> impl Iterator<Item = PoolObservation<'_>> {
        self.pools.iter().map(|(collateral_id, pool)| PoolObservation {
            collateral_id,
            gross: self.gross(collateral_id),
            reserved: pool.reserved,
            available: self.available(collateral_id),
            reports: pool.reports.len(),
            members: pool.members.len(),
            optimistic: pool.members.iter().filter(|key| self.is_optimistic(key)).count(),
        })
    }

    /// Replay for a newly subscribed peer, in spec order: membership, local collateral,
    /// promotion, then every retained report. Original fields and timestamps are preserved.
    pub(crate) fn replay(&self) -> Vec<OperatorMessage> {
        let mut out = Vec::new();
        for (collateral_id, pool) in &self.pools {
            if !pool.members.is_empty() {
                out.push(OperatorMessage::Membership(CollateralMembership {
                    ts_ms: pool.seen_ts,
                    collateral_id: collateral_id.clone(),
                    builder_pubkeys: pool.members.clone(),
                }));
            }
        }
        for (collateral_id, pool) in &self.pools {
            if let Some((collateral_wei, ts_ms)) = pool.collateral.get(&self.local_group) {
                out.push(OperatorMessage::Collateral(BuilderCollateral {
                    ts_ms: *ts_ms,
                    slot: 0,
                    collateral_id: collateral_id.clone(),
                    collateral_wei: *collateral_wei,
                    operator_group: Some(self.local_group.clone()),
                }));
            }
        }
        for pool in self.pools.values() {
            if let Some(promotion) = &pool.promotion {
                out.push(OperatorMessage::Promotion(promotion.clone()));
            }
        }
        for pool in self.pools.values() {
            out.extend(pool.reports.values().cloned().map(OperatorMessage::Demotion));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    const C1: &[u8] = b"C1";
    const C2: &[u8] = b"C2";
    const GROUP: &[u8] = b"op-a";
    const SLOT: u64 = 100;
    /// Every ordering of three messages.
    const PERMS: [[usize; 3]; 6] =
        [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];

    fn eth(n: u128) -> u128 {
        n * 1_000_000_000_000_000_000
    }

    fn state() -> PoolState {
        PoolState::new(GROUP.to_vec(), PromotionMode::Follow)
    }

    fn members(ts_ms: u64, id: &[u8], pubkeys: &[BlsPublicKeyBytes]) -> OperatorMessage {
        OperatorMessage::Membership(CollateralMembership {
            ts_ms,
            collateral_id: id.to_vec(),
            builder_pubkeys: pubkeys.to_vec(),
        })
    }

    fn backing(ts_ms: u64, id: &[u8], wei: u128) -> OperatorMessage {
        OperatorMessage::Collateral(BuilderCollateral {
            ts_ms,
            slot: 0,
            collateral_id: id.to_vec(),
            collateral_wei: wei,
            operator_group: Some(GROUP.to_vec()),
        })
    }

    fn promote(ts_ms: u64, id: &[u8], pubkey: BlsPublicKeyBytes) -> OperatorMessage {
        OperatorMessage::Promotion(Promotion {
            ts_ms,
            slot: SLOT,
            collateral_id: id.to_vec(),
            builder_pubkey: pubkey,
        })
    }

    fn demote(
        ts_ms: u64,
        slot: u64,
        id: &[u8],
        pubkey: BlsPublicKeyBytes,
        wei: u128,
    ) -> OperatorMessage {
        OperatorMessage::Demotion(Demotion {
            ts_ms,
            slot,
            collateral_id: id.to_vec(),
            builder_pubkey: pubkey,
            block_hash: B256::repeat_byte(ts_ms as u8),
            bid_value_wei: wei,
            reason_msg: b"invalid block".to_vec(),
        })
    }

    fn apply(state: &mut PoolState, msg: &OperatorMessage) -> Result<bool, PoolError> {
        match msg {
            OperatorMessage::Membership(m) => state.apply_membership(m),
            OperatorMessage::Collateral(c) => state.apply_collateral(GROUP.to_vec(), c),
            OperatorMessage::Promotion(p) => state.apply_promotion(p),
            OperatorMessage::Demotion(d) => state.apply_demotion(d),
            OperatorMessage::Payload(_) => unreachable!("payloads are not pool state"),
        }
    }

    /// Spec example 1: 10 ETH backing, 3 and 4 ETH faults at S reserve 4; a further 1 ETH fault
    /// at S+1 brings reservations to 5, leaving 5 available. Independent of arrival order.
    #[test]
    fn per_slot_maxima_sum_across_slots_in_any_order() {
        let [k1, k2, k3] = [0; 3].map(|_| BlsPublicKeyBytes::random());

        for perm in PERMS {
            let mut state = state();
            for msg in [members(1, C1, &[k1, k2, k3]), backing(1, C1, eth(10)), promote(2, C1, k1)]
            {
                apply(&mut state, &msg).unwrap();
            }

            let reports = [
                demote(10, SLOT, C1, k1, eth(3)),
                demote(11, SLOT, C1, k2, eth(4)),
                demote(12, SLOT + 1, C1, k3, eth(1)),
            ];
            for i in perm {
                assert!(apply(&mut state, &reports[i]).unwrap());
            }

            assert_eq!(state.reserved(C1), U256::from(eth(5)), "order {perm:?}");
            assert_eq!(state.available(C1), U256::from(eth(5)), "order {perm:?}");
            assert!(!state.is_optimistic(&k1));
            assert!(!state.is_optimistic(&k2));
            assert!(!state.is_optimistic(&k3));
        }
    }

    /// Spec example 2: a 7 ETH report at 100, a promotion at 101 and a 1 ETH report at 102 leave
    /// 1 ETH reserved and only the key reported at 102 pessimistic. Holds with the promotion last,
    /// which is why the smaller report must be retained.
    #[test]
    fn promotion_supersedes_only_older_reports_in_any_order() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());

        for perm in PERMS {
            let mut state = state();
            for msg in [members(1, C1, &[k1, k2]), backing(1, C1, eth(10))] {
                apply(&mut state, &msg).unwrap();
            }

            let msgs = [
                demote(100, SLOT, C1, k1, eth(7)),
                promote(101, C1, k1),
                demote(102, SLOT, C1, k2, eth(1)),
            ];
            for i in perm {
                apply(&mut state, &msgs[i]).unwrap();
            }

            assert_eq!(state.reserved(C1), U256::from(eth(1)), "order {perm:?}");
            assert_eq!(state.available(C1), U256::from(eth(9)), "order {perm:?}");
            assert!(state.is_optimistic(&k1), "order {perm:?}");
            assert!(!state.is_optimistic(&k2), "order {perm:?}");
        }
    }

    /// `Share` sums one contribution per operator group. Contributions from one group replace,
    /// they are never summed.
    #[test]
    fn share_sums_one_contribution_per_operator_group() {
        let k1 = BlsPublicKeyBytes::random();
        let build = |mode| {
            let mut pool = PoolState::new(GROUP.to_vec(), mode);
            apply(&mut pool, &members(1, C1, &[k1])).unwrap();
            let OperatorMessage::Collateral(local) = backing(1, C1, eth(10)) else {
                unreachable!()
            };
            pool.apply_collateral(GROUP.to_vec(), &local).unwrap();
            for (ts, wei) in [(1, eth(4)), (2, eth(6))] {
                let OperatorMessage::Collateral(remote) = backing(ts, C1, wei) else {
                    unreachable!()
                };
                pool.apply_collateral(b"op-b".to_vec(), &remote).unwrap();
            }
            pool
        };

        assert_eq!(build(PromotionMode::Follow).gross(C1), U256::from(eth(10)));
        assert_eq!(build(PromotionMode::Share).gross(C1), U256::from(eth(16)));
    }

    /// The budget is stored once per pool; only the reported bit is per pubkey.
    #[test]
    fn pool_budget_is_shared_and_only_the_reported_bit_is_per_key() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let mut pool = state();
        for msg in [
            members(1, C1, &[k1, k2]),
            backing(1, C1, eth(10)),
            promote(2, C1, k1),
            demote(10, SLOT, C1, k1, eth(3)),
        ] {
            apply(&mut pool, &msg).unwrap();
        }

        let pools: Vec<_> = pool.pool_records().collect();
        assert_eq!(pools.len(), 1, "one budget record for the pool, not one per member");
        let (index, _, available, promoted) = pools[0].clone();
        assert_eq!(available, U256::from(eth(7)));
        assert!(promoted);

        let keys: FxHashMap<_, _> =
            pool.key_records().map(|(key, pool, reported)| (key, (pool, reported))).collect();
        assert_eq!(keys[&k1], (index, true), "reported pubkey");
        assert_eq!(keys[&k2], (index, false), "clean member");
    }

    #[test]
    fn binding_is_write_once() {
        let k1 = BlsPublicKeyBytes::random();
        let mut state = state();
        apply(&mut state, &members(1, C1, &[k1])).unwrap();

        let conflict = state.apply_membership(&CollateralMembership {
            ts_ms: 2,
            collateral_id: C2.to_vec(),
            builder_pubkeys: vec![k1],
        });
        assert!(matches!(conflict, Err(PoolError::BindingConflict { .. })));
        assert_eq!(state.binding[&k1], C1.to_vec());
    }

    /// One conflicting pubkey rejects the whole set: a partial accept would store something the
    /// publisher did not send.
    #[test]
    fn membership_conflict_rejects_the_whole_set() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let mut state = state();
        apply(&mut state, &members(1, C1, &[k1])).unwrap();

        let conflict = state.apply_membership(&CollateralMembership {
            ts_ms: 2,
            collateral_id: C2.to_vec(),
            builder_pubkeys: vec![k1, k2],
        });
        assert!(matches!(conflict, Err(PoolError::BindingConflict { .. })));
        assert!(!state.binding.contains_key(&k2), "k2 must not be bound by a rejected set");
    }

    #[test]
    fn newer_set_may_not_drop_a_bound_pubkey() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let mut state = state();
        apply(&mut state, &members(1, C1, &[k1, k2])).unwrap();

        let removal = state.apply_membership(&CollateralMembership {
            ts_ms: 2,
            collateral_id: C1.to_vec(),
            builder_pubkeys: vec![k1],
        });
        assert!(matches!(removal, Err(PoolError::MembershipRemoval { .. })));
        assert_eq!(state.pools[C1].members.len(), 2);
    }

    /// A stale set legitimately omits pubkeys added after it was published.
    #[test]
    fn stale_set_omitting_a_later_pubkey_is_accepted() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let mut state = state();
        apply(&mut state, &members(5, C1, &[k1, k2])).unwrap();

        assert!(!apply(&mut state, &members(1, C1, &[k1])).unwrap());
        assert_eq!(state.pools[C1].members.len(), 2);
        assert_eq!(state.pools[C1].seen_ts, 5);
    }

    /// Retained and inert until the pool is known. Dropping it would make state order-dependent.
    #[test]
    fn report_for_unknown_pool_is_retained_and_becomes_effective() {
        let k1 = BlsPublicKeyBytes::random();
        let mut state = state();

        assert!(apply(&mut state, &demote(10, SLOT, C1, k1, eth(3))).unwrap());
        assert_eq!(state.reserved(C1), U256::from(eth(3)));
        assert_eq!(state.available(C1), U256::ZERO, "no backing yet");
        assert!(!state.is_optimistic(&k1));

        for msg in [members(1, C1, &[k1]), backing(1, C1, eth(10)), promote(2, C1, k1)] {
            apply(&mut state, &msg).unwrap();
        }
        assert_eq!(state.available(C1), U256::from(eth(7)));
        assert!(!state.is_optimistic(&k1), "the retained report still demotes k1");
    }

    /// Same identity, conflicting value: the larger wins, so the result is order-independent.
    #[test]
    fn duplicate_report_keeps_the_larger_value() {
        let k1 = BlsPublicKeyBytes::random();
        let block_hash = B256::repeat_byte(7);
        let report = |wei| Demotion {
            ts_ms: 10,
            slot: SLOT,
            collateral_id: C1.to_vec(),
            builder_pubkey: k1,
            block_hash,
            bid_value_wei: wei,
            reason_msg: Vec::new(),
        };

        for [first, second] in [[eth(3), eth(5)], [eth(5), eth(3)]] {
            let mut state = state();
            assert!(state.apply_demotion(&report(first)).unwrap());
            state.apply_demotion(&report(second)).unwrap();
            assert_eq!(state.pools[C1].reports.len(), 1);
            assert_eq!(state.reserved(C1), U256::from(eth(5)));
        }
    }

    #[test]
    fn replay_reproduces_state_in_any_order() {
        let [k1, k2] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let mut source = state();
        for msg in [
            members(1, C1, &[k1, k2]),
            backing(1, C1, eth(10)),
            promote(2, C1, k1),
            demote(10, SLOT, C1, k1, eth(3)),
            demote(11, SLOT, C1, k2, eth(4)),
            demote(12, SLOT + 1, C1, k1, eth(1)),
        ] {
            apply(&mut source, &msg).unwrap();
        }

        let mut replay = source.replay();
        assert_eq!(replay.len(), 6);
        replay.reverse();

        let mut target = state();
        for msg in &replay {
            apply(&mut target, msg).unwrap();
        }

        assert_eq!(target.reserved(C1), source.reserved(C1));
        assert_eq!(target.available(C1), source.available(C1));
        assert_eq!(target.pools[C1].reports.len(), source.pools[C1].reports.len());
        assert_eq!(target.is_optimistic(&k1), source.is_optimistic(&k1));
        assert_eq!(target.is_optimistic(&k2), source.is_optimistic(&k2));
    }
}
