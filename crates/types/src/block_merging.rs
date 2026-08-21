use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use bitflags::bitflags;
use rand::Rng;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use ssz::{Decode, Encode};
use ssz_derive::{Decode, Encode};

use crate::{
    Blob, SignedBidSubmission, TestRandom,
    fields::{KzgCommitment, KzgProof},
};

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignedBidSubmissionWithMergingData {
    pub submission: SignedBidSubmission,
    pub merging_data: BlockMergingData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[ssz(enum_behaviour = "union")]
#[serde(untagged)]
pub enum Order {
    Tx(TransactionOrder),
    Bundle(BundleOrder),
    /// A bundle carrying merge flags (e.g. `LATEST_ONLY`). Kept as a distinct variant rather than
    /// a new field on [`BundleOrder`] so that old wire bytes for `Order::Bundle` — encoded before
    /// this variant existed — keep decoding unchanged: an SSZ union's selector byte makes new
    /// variants additive, whereas a new field changes the container's byte layout.
    BundleV2(BundleOrderV2),
}

impl TestRandom for Order {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        match rng.random_range(0..3) {
            0 => Order::Tx(TransactionOrder::random_for_test(rng)),
            1 => Order::Bundle(BundleOrder::random_for_test(rng)),
            _ => Order::BundleV2(BundleOrderV2::random_for_test(rng)),
        }
    }
}

/// Vector of transaction indices. Aliased for easier use.
pub type TxIndices = SmallVec<[usize; 8]>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransactionOrder {
    /// Index into block.transactions
    pub index: usize,
    /// If the transaction is allowed to revert
    pub can_revert: bool,
}

impl TestRandom for TransactionOrder {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self { index: usize::random_for_test(rng), can_revert: bool::random_for_test(rng) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// References a bundle of transactions via their indices.
/// All indices are for the block the transactions come from.
pub struct BundleOrder {
    /// Signals txs that are part of the bundle and ordering of txs.
    /// Indices are for the block body the transactions come from.
    pub txs: TxIndices,
    /// Txs that may revert.
    /// Indices are for the [txs](Self::txs) array.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    /// Indices are for the [txs](Self::txs) array.
    pub dropping_txs: TxIndices,
}

impl TestRandom for BundleOrder {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            txs: TxIndices::random_for_test(rng),
            reverting_txs: TxIndices::random_for_test(rng),
            dropping_txs: TxIndices::random_for_test(rng),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Same as [`BundleOrder`], with the addition of `flags`. See [`Order::BundleV2`] for why
/// this is a separate type rather than a field on `BundleOrder`.
pub struct BundleOrderV2 {
    /// Signals txs that are part of the bundle and ordering of txs.
    /// Indices are for the block body the transactions come from.
    pub txs: TxIndices,
    /// Txs that may revert.
    /// Indices are for the [txs](Self::txs) array.
    pub reverting_txs: TxIndices,
    /// Txs that are allowed to be omitted, but not revert.
    /// Indices are for the [txs](Self::txs) array.
    pub dropping_txs: TxIndices,
    pub flags: MergeOrderFlags,
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct MergeOrderFlags: u64 {
        /// Only eligible for merging if present in the builder's most recent submission for this
        /// block.
        const LATEST_ONLY = 1 << 0;
    }
}

impl TestRandom for MergeOrderFlags {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self::from_bits_retain(u64::random_for_test(rng))
    }
}

/// SSZ wire bytes are the raw bit pattern; unknown bits round-trip rather than erroring, so a
/// relay/builder pair with mismatched flag sets stays compatible.
impl Encode for MergeOrderFlags {
    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        8
    }

    fn ssz_bytes_len(&self) -> usize {
        8
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        self.bits().ssz_append(buf);
    }
}

impl Decode for MergeOrderFlags {
    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        8
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        <u64 as Decode>::from_ssz_bytes(bytes).map(Self::from_bits_retain)
    }
}

impl TestRandom for BundleOrderV2 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            txs: TxIndices::random_for_test(rng),
            reverting_txs: TxIndices::random_for_test(rng),
            dropping_txs: TxIndices::random_for_test(rng),
            flags: MergeOrderFlags::random_for_test(rng),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InvalidTxIndex;

impl BundleOrder {
    pub fn validate(&self) -> Result<(), InvalidTxIndex> {
        if self.reverting_txs.iter().any(|&i| i >= self.txs.len()) ||
            self.dropping_txs.iter().any(|&i| i >= self.txs.len())
        {
            return Err(InvalidTxIndex);
        }
        Ok(())
    }
}

impl BundleOrderV2 {
    pub fn validate(&self) -> Result<(), InvalidTxIndex> {
        if self.reverting_txs.iter().any(|&i| i >= self.txs.len()) ||
            self.dropping_txs.iter().any(|&i| i >= self.txs.len())
        {
            return Err(InvalidTxIndex);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BlockMergingData {
    pub allow_appending: bool,
    pub builder_address: Address,
    pub merge_orders: Vec<Order>,
}

impl TestRandom for BlockMergingData {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            allow_appending: bool::random_for_test(rng),
            builder_address: Address::random_for_test(rng),
            merge_orders: Vec::random_for_test(rng),
        }
    }
}

impl BlockMergingData {
    pub fn allow_all(builder_address: Address, num_tx: usize) -> Self {
        let mut merge_orders = Vec::with_capacity(num_tx);
        for index in 0..num_tx {
            merge_orders.push(Order::Tx(TransactionOrder { index, can_revert: true }));
        }
        Self { allow_appending: true, builder_address, merge_orders }
    }

    pub fn append_only(builder_address: Address) -> Self {
        Self { allow_appending: true, builder_address, merge_orders: vec![] }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlobWithMetadata {
    pub commitment: KzgCommitment,
    pub proofs: Vec<KzgProof>,
    pub blob: Blob,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuilderInclusionResult {
    /// Value merged in from this builder's block.
    #[serde(with = "serde_utils::quoted_u256", default)]
    pub contribution: U256,
    /// What this builder earned for its contribution, net of payout-tx gas —
    /// matches the on-chain split.
    #[serde(with = "serde_utils::quoted_u256", default)]
    pub revenue: U256,
    pub txs: Vec<B256>,
}

#[derive(Debug, Clone, Default)]
pub struct MergedBlock {
    pub slot: u64,
    pub block_number: u64,
    pub original_block_hash: B256,
    pub block_hash: B256,
    pub original_value: U256,
    pub proposer_value: U256,
    /// Total value added by the merged orders — sum of `contribution` across
    /// `builder_inclusions`.
    pub total_merged_value: U256,
    pub base_builder_revenue: U256,
    pub relay_revenue: U256,
    pub original_tx_count: usize,
    pub merged_tx_count: usize,
    pub original_blob_count: usize,
    pub merged_blob_count: usize,
    pub original_gas_used: u64,
    pub merged_gas_used: u64,
    pub builder_inclusions: HashMap<Address, BuilderInclusionResult>,
    pub trace: MergedBlockTrace,
}

impl MergedBlock {
    pub fn block_hash(&self) -> B256 {
        self.block_hash
    }

    pub fn to_alert_message(&self) -> String {
        let builder_inclusions = self
            .builder_inclusions
            .iter()
            .map(|(address, res)| format!("*{}*: {}", res.txs.len(), address,))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "📦 *Merged Block Delivered*\n\
                \n\
                *Slot:* `{}`\n\
                *Block Number:* `{}`\n\
                *Block Hash:* `{}`\n\
                *Value:* `{}` → `{}`\n\
                *Transactions:* `{}` → `{}`\n\
                *Blobs:* `{}` → `{}`\n\
                \n\
                ━━━━━━━━━━━━━━━\n\
                *Merged txs by builder:*\n{}\n\
                ━━━━━━━━━━━━━━━\n",
            self.slot,
            self.block_number,
            self.block_hash,
            self.original_value,
            self.proposer_value,
            self.original_tx_count,
            self.merged_tx_count,
            self.original_blob_count,
            self.merged_blob_count,
            builder_inclusions
        )
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergedBlockTrace {
    pub request_time_ns: u64,
    pub sim_start_time_ns: u64,
    pub sim_end_time_ns: u64,
    pub finalize_time_ns: u64,
    pub header_served_time_ns: Option<u64>,
    /// Whether this block was the highest-value merged block across all builders at the time
    /// its header was served. `None` until a header is served for it — most merged blocks in
    /// the table are never served, so this is left unset for them.
    pub was_top_builder: Option<bool>,
    /// Value of the bid that beat this one, set when a mev-boost `get_header` call found this
    /// merged bid was not higher than the original (unmerged) bid it was competing with.
    pub top_bid: Option<U256>,
}

#[cfg(test)]
mod tests {
    use ssz::{Decode, Encode};

    use super::*;

    #[test]
    fn order_ssz_round_trip_tx() {
        let tx_order = TransactionOrder { index: 42, can_revert: true };
        let order = Order::Tx(tx_order);

        let bytes = order.as_ssz_bytes();
        let decoded = Order::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn bundle_order_ssz_round_trip() {
        let bundle_order = BundleOrder {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
        };

        let bytes = bundle_order.as_ssz_bytes();
        let decoded = BundleOrder::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(bundle_order, decoded);
    }

    #[test]
    fn bundle_order_v2_ssz_round_trip() {
        let bundle_order = BundleOrderV2 {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
            flags: MergeOrderFlags::LATEST_ONLY,
        };

        let bytes = bundle_order.as_ssz_bytes();
        let decoded = BundleOrderV2::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(bundle_order, decoded);
    }

    #[test]
    fn order_ssz_round_trip_bundle() {
        let bundle_order = BundleOrder {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
        };
        let order = Order::Bundle(bundle_order);

        let bytes = order.as_ssz_bytes();
        let decoded = Order::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn order_ssz_round_trip_bundle_v2() {
        let bundle_order = BundleOrderV2 {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
            flags: MergeOrderFlags::LATEST_ONLY,
        };
        let order = Order::BundleV2(bundle_order);

        let bytes = order.as_ssz_bytes();
        let decoded = Order::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn order_ssz_bundle_and_bundle_v2_have_distinct_selectors() {
        // The union selector byte is what makes adding `BundleV2` backward compatible: bytes
        // already on the wire as `Order::Bundle` (selector 1) must never be mistaken for the
        // new `Order::BundleV2` (selector 2), and vice versa.
        let bundle = Order::Bundle(BundleOrder {
            txs: smallvec::smallvec![1],
            reverting_txs: smallvec::smallvec![],
            dropping_txs: smallvec::smallvec![],
        });
        let bundle_v2 = Order::BundleV2(BundleOrderV2 {
            txs: smallvec::smallvec![1],
            reverting_txs: smallvec::smallvec![],
            dropping_txs: smallvec::smallvec![],
            flags: MergeOrderFlags::LATEST_ONLY,
        });

        assert_eq!(bundle.as_ssz_bytes()[0], 1);
        assert_eq!(bundle_v2.as_ssz_bytes()[0], 2);

        // Old bytes for `Order::Bundle` still decode to `Order::Bundle`, not `BundleV2`.
        let decoded = Order::from_ssz_bytes(&bundle.as_ssz_bytes()).unwrap();
        assert!(matches!(decoded, Order::Bundle(_)));
    }

    #[test]
    fn block_merging_data_ssz_round_trip_mixed_orders() {
        let tx_order = Order::Tx(TransactionOrder { index: 1, can_revert: false });
        let bundle_order = Order::Bundle(BundleOrder {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
        });
        let bundle_v2_order = Order::BundleV2(BundleOrderV2 {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
            flags: MergeOrderFlags::LATEST_ONLY,
        });

        let data = BlockMergingData {
            allow_appending: true,
            builder_address: Address::repeat_byte(0x42),
            merge_orders: vec![tx_order, bundle_order, bundle_v2_order],
        };

        let bytes = data.as_ssz_bytes();
        let decoded = BlockMergingData::from_ssz_bytes(&bytes).expect("SSZ decode should succeed");
        assert_eq!(data, decoded);
    }

    #[test]
    fn order_json_round_trip_tx() {
        let tx_order = TransactionOrder { index: 42, can_revert: true };
        let order = Order::Tx(tx_order);

        let json = serde_json::to_string(&order).expect("JSON encode should succeed");
        let decoded: Order = serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    fn order_json_round_trip_bundle() {
        let bundle_order = BundleOrder {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
        };
        let order = Order::Bundle(bundle_order);

        let json = serde_json::to_string(&order).expect("JSON encode should succeed");
        let decoded: Order = serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    #[ignore = "known bug: Order::BundleV2 not disambiguated from Order::Bundle by untagged JSON, see gattaca-com/helix#486"]
    fn order_json_round_trip_bundle_v2() {
        let bundle_order = BundleOrderV2 {
            txs: smallvec::smallvec![1, 2, 3],
            reverting_txs: smallvec::smallvec![0],
            dropping_txs: smallvec::smallvec![2],
            flags: MergeOrderFlags::LATEST_ONLY,
        };
        let order = Order::BundleV2(bundle_order);

        let json = serde_json::to_string(&order).expect("JSON encode should succeed");
        let decoded: Order = serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(order, decoded);
    }

    #[test]
    #[ignore = "known bug: Order::BundleV2 not disambiguated from Order::Bundle by untagged JSON, see gattaca-com/helix#486"]
    fn order_json_untagged_disambiguates_bundle_from_bundle_v2() {
        // `Order` is `#[serde(untagged)]`, so `Bundle` and `BundleV2` must remain
        // distinguishable by shape alone: `BundleV2` requires `flags`
        // (no `#[serde(default)]`), so its presence/absence is the discriminator.
        let without_flag = r#"{"txs": [1], "reverting_txs": [], "dropping_txs": []}"#;
        let with_flag_true =
            r#"{"txs": [1], "reverting_txs": [], "dropping_txs": [], "flags": "LATEST_ONLY"}"#;
        // Presence of the key routes to `BundleV2` regardless of its value — `deny_unknown_fields`
        // rejects `BundleOrder` here even though the flag set is empty, so it never falls back
        // silently and loses the field.
        let with_flag_false =
            r#"{"txs": [1], "reverting_txs": [], "dropping_txs": [], "flags": ""}"#;

        let decoded: Order = serde_json::from_str(without_flag).unwrap();
        assert!(matches!(decoded, Order::Bundle(_)));

        let decoded: Order = serde_json::from_str(with_flag_true).unwrap();
        assert!(matches!(decoded, Order::BundleV2(_)));

        let decoded: Order = serde_json::from_str(with_flag_false).unwrap();
        assert!(matches!(decoded, Order::BundleV2(_)));
    }

    #[test]
    #[ignore = "known bug: Order::BundleV2 not disambiguated from Order::Bundle by untagged JSON, see gattaca-com/helix#486"]
    fn block_merging_data_json_round_trip_mixed_orders() {
        let tx_order = Order::Tx(TransactionOrder { index: 1, can_revert: false });
        let bundle_order = Order::Bundle(BundleOrder {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
        });
        let bundle_v2_order = Order::BundleV2(BundleOrderV2 {
            txs: smallvec::smallvec![0, 2, 4],
            reverting_txs: smallvec::smallvec![1],
            dropping_txs: smallvec::smallvec![2],
            flags: MergeOrderFlags::LATEST_ONLY,
        });

        let data = BlockMergingData {
            allow_appending: true,
            builder_address: Address::repeat_byte(0x42),
            merge_orders: vec![tx_order, bundle_order, bundle_v2_order],
        };

        let json = serde_json::to_string(&data).expect("JSON encode should succeed");
        let decoded: BlockMergingData =
            serde_json::from_str(&json).expect("JSON decode should succeed");
        assert_eq!(data, decoded);
    }
}
