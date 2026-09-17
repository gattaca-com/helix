use std::sync::Arc;

use helix_types::{
    BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, MergeType, SignedBidSubmission,
    Transaction,
    rebid::{RebidError, RebidV1},
};
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use ssz::{Decode, Encode};

#[derive(Clone, Debug)]
pub struct CachedRebidBase {
    pub submission: SignedBidSubmission,
    pub merging_data: Option<BlockMergingData>,
    pub merge_type: MergeType,
    pub with_adjustments: bool,
    pub adjustment_version: u8,
}

impl CachedRebidBase {
    pub fn apply(
        &self,
        patch: &RebidV1,
        merge_type: MergeType,
        with_adjustments: bool,
    ) -> Result<(SignedBidSubmission, Option<BidAdjustmentData>), RebidError> {
        if patch.version != 1 ||
            patch.slot != self.submission.message.slot ||
            self.merge_type != merge_type ||
            self.with_adjustments != with_adjustments ||
            patch.payment_transaction.is_empty() ||
            patch.payment_transaction.len() > 1_048_576 ||
            with_adjustments == patch.adjustment.is_empty()
        {
            return Err(RebidError::InvalidPatch);
        }
        let adjustment = if with_adjustments {
            Some(
                BidAdjustmentData::from_ssz_bytes(&patch.adjustment)
                    .map_err(|_| RebidError::InvalidPatch)?,
            )
        } else {
            None
        };
        let version = match &adjustment {
            None => 0,
            Some(BidAdjustmentData::V1(_)) => 1,
            Some(BidAdjustmentData::V2(_)) => 2,
        };
        if version != self.adjustment_version {
            return Err(RebidError::InvalidPatch);
        }
        let mut submission = self.submission.clone();
        let payload = submission.execution_payload_make_mut();
        let last = payload.transactions.last_mut().ok_or(RebidError::InvalidPatch)?;
        *last = Transaction(patch.payment_transaction.clone().into());
        payload.state_root = patch.state_root.into();
        payload.block_hash = patch.block_hash.into();
        submission.message.block_hash = patch.block_hash.into();
        submission.message.value = alloy_primitives::U256::from_le_bytes(patch.value);
        submission.signature = patch.signature.into();
        Ok((submission, adjustment))
    }
}

#[derive(Default)]
struct CacheState {
    next_session: u64,
    sessions: FxHashSet<u64>,
    slot: u64,
    entries: FxHashMap<(u64, [u8; 32]), (Arc<CachedRebidBase>, usize, u64)>,
    bytes: usize,
    next_entry: u64,
}

#[derive(Default)]
pub struct RebidCache {
    inner: RwLock<CacheState>,
}

impl RebidCache {
    pub fn connect(&self) -> u64 {
        let mut state = self.inner.write();
        state.next_session =
            state.next_session.checked_add(1).expect("connection generation exhausted");
        let session = state.next_session;
        state.sessions.insert(session);
        session
    }

    pub fn disconnect(&self, session: u64) {
        let mut state = self.inner.write();
        state.sessions.remove(&session);
        let mut removed = 0;
        state.entries.retain(|(s, _), (_, bytes, _)| {
            if *s == session {
                removed += *bytes;
                false
            } else {
                true
            }
        });
        state.bytes -= removed;
    }

    pub fn new_slot(&self, slot: u64) {
        let mut state = self.inner.write();
        if slot > state.slot {
            state.entries.clear();
            state.bytes = 0;
            state.slot = slot;
        }
    }

    pub fn get(
        &self,
        session: u64,
        id: [u8; 32],
        slot: u64,
        pubkey: &BlsPublicKeyBytes,
    ) -> Result<Arc<CachedRebidBase>, RebidError> {
        let state = self.inner.read();
        let (base, _, _) = state.entries.get(&(session, id)).ok_or(RebidError::MissingBase)?;
        if base.submission.message.slot != slot || base.submission.message.builder_pubkey != *pubkey
        {
            return Err(RebidError::MissingBase);
        }
        Ok(base.clone())
    }

    pub fn insert(
        &self,
        session: u64,
        id: [u8; 32],
        base: CachedRebidBase,
    ) -> Result<(), RebidError> {
        let bytes = base.submission.ssz_bytes_len() +
            base.submission.execution_payload.transactions.len() * 32 +
            base.merging_data.as_ref().map_or(0, Encode::ssz_bytes_len);
        let mut state = self.inner.write();
        if !state.sessions.contains(&session) || base.submission.message.slot != state.slot {
            return Err(RebidError::MissingBase);
        }
        if base.submission.execution_payload.transactions.is_empty() {
            return Err(RebidError::InvalidPatch);
        }
        if let Some((old, _, _)) = state.entries.get(&(session, id)) {
            let mut candidate = base.submission.clone();
            candidate.message.block_hash = old.submission.message.block_hash;
            candidate.message.value = old.submission.message.value;
            let payload = candidate.execution_payload_make_mut();
            payload.block_hash = old.submission.execution_payload.block_hash;
            payload.state_root = old.submission.execution_payload.state_root;
            *payload.transactions.last_mut().unwrap() =
                old.submission.execution_payload.transactions.last().unwrap().clone();
            return if old.merge_type == base.merge_type &&
                old.with_adjustments == base.with_adjustments &&
                old.adjustment_version == base.adjustment_version &&
                old.merging_data == base.merging_data &&
                candidate.message == old.submission.message &&
                candidate.execution_payload == old.submission.execution_payload &&
                candidate.blobs_bundle == old.submission.blobs_bundle &&
                candidate.execution_requests == old.submission.execution_requests
            {
                Ok(())
            } else {
                Err(RebidError::ConflictingBase)
            };
        }
        if bytes > 256 * 1024 * 1024 {
            return Err(RebidError::CacheFull);
        }
        loop {
            let mut builder_count = 0;
            let mut builder_bytes = bytes;
            for (entry, size, _) in state.entries.values() {
                if entry.submission.message.builder_pubkey == base.submission.message.builder_pubkey
                {
                    builder_count += 1;
                    builder_bytes += size;
                }
            }
            let builder_full = builder_count >= 64 || builder_bytes > 256 * 1024 * 1024;
            if !builder_full &&
                state.entries.len() < 1024 &&
                state.bytes + bytes <= 1024 * 1024 * 1024
            {
                break;
            }
            let victim = state
                .entries
                .iter()
                .filter(|(_, (entry, _, _))| {
                    !builder_full ||
                        entry.submission.message.builder_pubkey ==
                            base.submission.message.builder_pubkey
                })
                .min_by_key(|(_, (_, _, age))| *age)
                .map(|(key, _)| *key)
                .ok_or(RebidError::CacheFull)?;
            let (_, size, _) = state.entries.remove(&victim).unwrap();
            state.bytes -= size;
        }
        let age = state.next_entry;
        state.next_entry += 1;
        state.entries.insert((session, id), (Arc::new(base), bytes, age));
        state.bytes += bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use helix_types::{Submission, TestRandom, Transaction};
    use ssz::Encode;

    use super::*;

    fn base() -> CachedRebidBase {
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        submission.message.slot = 42;
        submission.execution_payload_make_mut().transactions =
            vec![Transaction(vec![1; 100].into()), Transaction(vec![2; 100].into())]
                .try_into()
                .unwrap();
        CachedRebidBase {
            submission,
            merging_data: None,
            merge_type: MergeType::None,
            with_adjustments: false,
            adjustment_version: 0,
        }
    }

    #[test]
    fn rebids_are_independent_and_leave_base_immutable() {
        let base = base();
        let original = base.submission.as_ssz_bytes();
        let mut patch = RebidV1 {
            version: 1,
            slot: 42,
            base_id: [0; 32],
            value: [0; 32],
            block_hash: [3; 32],
            state_root: [4; 32],
            signature: [5; 96],
            payment_transaction: vec![6; 120],
            adjustment: vec![],
        };
        let (first, _) = base.apply(&patch, MergeType::None, false).unwrap();
        patch.value[0] = 123;
        patch.payment_transaction = vec![7; 130];
        let (second, _) = base.apply(&patch, MergeType::None, false).unwrap();
        assert_eq!(base.submission.as_ssz_bytes(), original);
        assert_eq!(first.execution_payload.transactions[1].as_ref(), &[6; 120]);
        assert_eq!(second.execution_payload.transactions[1].as_ref(), &[7; 130]);
        assert_eq!(
            second.execution_payload.transactions[0],
            base.submission.execution_payload.transactions[0]
        );
        assert_eq!(second.message.value, alloy_primitives::U256::from(123));
        assert!(matches!(Submission::Full(second), Submission::Full(_)));
        assert_eq!(
            base.apply(&patch, MergeType::Mergeable, false).unwrap_err(),
            RebidError::InvalidPatch
        );
        patch.slot += 1;
        assert_eq!(
            base.apply(&patch, MergeType::None, false).unwrap_err(),
            RebidError::InvalidPatch
        );
    }

    #[test]
    fn cache_is_scoped_bounded_and_does_not_resurrect_closed_connections() {
        let cache = RebidCache::default();
        cache.new_slot(42);
        let session = cache.connect();
        let base = base();
        let pubkey = base.submission.message.builder_pubkey;
        cache.insert(session, [1; 32], base.clone()).unwrap();
        let retained = cache.get(session, [1; 32], 42, &pubkey).unwrap();
        assert!(cache.get(session, [1; 32], 43, &pubkey).is_err());
        assert!(cache.get(session + 1, [1; 32], 42, &pubkey).is_err());
        assert!(cache.get(session, [1; 32], 42, &Default::default()).is_err());
        let mut conflicting = base.clone();
        conflicting.submission.message.parent_hash = alloy_primitives::B256::ZERO;
        assert_eq!(cache.insert(session, [1; 32], conflicting), Err(RebidError::ConflictingBase));
        let mut different_bid = base.clone();
        different_bid.submission.message.value = alloy_primitives::U256::ZERO;
        different_bid.submission.message.block_hash = alloy_primitives::B256::ZERO;
        cache.insert(session, [1; 32], different_bid).unwrap();
        cache.disconnect(session);
        assert_eq!(cache.insert(session, [2; 32], base.clone()), Err(RebidError::MissingBase));
        assert_eq!(retained.submission.message.slot, 42);
        let session = cache.connect();
        cache.insert(session, [1; 32], base).unwrap();
        for id in 2..=65 {
            cache.insert(session, [id; 32], retained.as_ref().clone()).unwrap();
        }
        assert!(cache.get(session, [1; 32], 42, &pubkey).is_err());
        assert!(cache.get(session, [65; 32], 42, &pubkey).is_ok());
        assert_eq!(cache.inner.read().entries.len(), 64);
        cache.new_slot(43);
        assert!(cache.get(session, [1; 32], 42, &pubkey).is_err());
    }
    #[test]
    fn rebid_adjustment_versions_and_empty_payment_are_checked() {
        use helix_types::{BidAdjData, BidAdjDataV2, BidAdjustmentDataV1, BidAdjustmentDataV2};
        let mut base = base();
        base.with_adjustments = true;
        let mut patch = RebidV1 {
            version: 1,
            slot: 42,
            base_id: [0; 32],
            value: [0; 32],
            block_hash: [3; 32],
            state_root: [4; 32],
            signature: [5; 96],
            payment_transaction: vec![6; 120],
            adjustment: vec![],
        };
        let versions = [
            BidAdjustmentData::V1(BidAdjustmentDataV1::Original(BidAdjData::default())),
            BidAdjustmentData::V2(BidAdjustmentDataV2::Original(BidAdjDataV2::default())),
        ];
        for (index, adjustment) in versions.into_iter().enumerate() {
            base.adjustment_version = index as u8 + 1;
            patch.adjustment = adjustment.as_ssz_bytes();
            let (_, result) = base.apply(&patch, MergeType::None, true).unwrap();
            assert_eq!(result, Some(adjustment));
            assert!(base.apply(&patch, MergeType::None, false).is_err());
        }
        patch.payment_transaction.clear();
        assert!(base.apply(&patch, MergeType::None, true).is_err());
        patch.payment_transaction.push(1);
        patch.version = 2;
        assert!(base.apply(&patch, MergeType::None, true).is_err());
    }
}
