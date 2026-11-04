use std::iter;

use alloy_primitives::{Address, B256, U256};
use lh_types::{EthSpec, ForkName, ForkVersionDecode, MainnetEthSpec};
use serde::{Deserialize, Serialize};
use silver_ssz::{
    merkle::{
        self, ZERO_HASH, ZERO_HASHES, hash_concat_many, mix_in_length, pack_byte_chunks,
        uint64_chunk,
    },
    ssz_view::{
        MAX_BYTES_PER_TRANSACTION, MAX_TRANSACTIONS_PER_PAYLOAD, MAX_WITHDRAWALS_PER_PAYLOAD,
    },
};
use ssz::Decode;
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    BlockValidationError, SszError, TestRandom, convert_bloom_to_lighthouse,
    convert_transactions_to_lighthouse, convert_transactions_to_progressive,
    fields::{Bloom, ExtraData, Transaction, Transactions, Withdrawals},
};

const TRANSACTION_DEPTH: u32 = (MAX_BYTES_PER_TRANSACTION / 32).ilog2();
const TRANSACTIONS_DEPTH: u32 = MAX_TRANSACTIONS_PER_PAYLOAD.ilog2();
const WITHDRAWALS_DEPTH: u32 = MAX_WITHDRAWALS_PER_PAYLOAD.ilog2();

// Both Electra and Fulu share the same ExecutionPayload,
// so we avoid wrapping it in an enum
/// From lighthouse, replacing a few List<u8>
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPayload {
    pub parent_hash: B256,
    pub fee_recipient: Address,
    pub state_root: B256,
    pub receipts_root: B256,
    pub logs_bloom: Bloom, // replaced
    pub prev_randao: B256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub block_number: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub extra_data: ExtraData, // replaced
    #[serde(with = "serde_utils::quoted_u256")]
    pub base_fee_per_gas: U256,
    pub block_hash: B256,
    pub transactions: Transactions, // replaced
    pub withdrawals: Withdrawals,
    #[serde(with = "serde_utils::quoted_u64")]
    pub blob_gas_used: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub excess_blob_gas: u64,
}

impl TestRandom for ExecutionPayload {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            parent_hash: B256::random_for_test(rng),
            fee_recipient: Address::random_for_test(rng),
            state_root: B256::random_for_test(rng),
            receipts_root: B256::random_for_test(rng),
            logs_bloom: Bloom::random(),
            prev_randao: B256::random_for_test(rng),
            block_number: u64::random_for_test(rng),
            gas_limit: u64::random_for_test(rng),
            gas_used: u64::random_for_test(rng),
            timestamp: u64::random_for_test(rng),
            extra_data: ExtraData::default(),
            base_fee_per_gas: U256::random_for_test(rng),
            block_hash: B256::random_for_test(rng),
            transactions: Transactions::random_for_test(rng),
            withdrawals: Withdrawals::random_for_test(rng),
            blob_gas_used: u64::random_for_test(rng),
            excess_blob_gas: u64::random_for_test(rng),
        }
    }
}

impl ExecutionPayload {
    /// Since the variable list types are now wrapped in Bytes, we need to validate that the fields
    /// are of the correct length
    pub fn validate_ssz_lengths(&self) -> Result<(), BlockValidationError> {
        if self.extra_data.len() > <MainnetEthSpec as EthSpec>::max_extra_data_bytes() {
            return Err(BlockValidationError::SszError(SszError::InvalidByteCount {
                given: self.extra_data.len(),
                expected: <MainnetEthSpec as EthSpec>::max_extra_data_bytes(),
            }));
        }

        for tx in self.transactions.iter() {
            if tx.len() > <MainnetEthSpec as EthSpec>::max_bytes_per_transaction() {
                return Err(BlockValidationError::SszError(SszError::InvalidByteCount {
                    given: tx.len(),
                    expected: <MainnetEthSpec as EthSpec>::max_bytes_per_transaction(),
                }));
            }
        }

        if self.transactions.len() > <MainnetEthSpec as EthSpec>::max_transactions_per_payload() {
            return Err(BlockValidationError::SszError(SszError::InvalidByteCount {
                given: self.transactions.len(),
                expected: <MainnetEthSpec as EthSpec>::max_transactions_per_payload(),
            }));
        }

        Ok(())
    }

    pub fn to_header(
        &self,
        withdrawals_root: Option<B256>,
        transactions_root: Option<B256>,
    ) -> ExecutionPayloadHeader {
        ExecutionPayloadHeader {
            parent_hash: self.parent_hash,
            fee_recipient: self.fee_recipient,
            state_root: self.state_root,
            receipts_root: self.receipts_root,
            logs_bloom: self.logs_bloom,
            prev_randao: self.prev_randao,
            block_number: self.block_number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            extra_data: self.extra_data.clone(),
            base_fee_per_gas: self.base_fee_per_gas,
            block_hash: self.block_hash,
            transactions_root: transactions_root.unwrap_or_else(|| self.transaction_root()),
            withdrawals_root: withdrawals_root.unwrap_or_else(|| self.withdrawals_root()),
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
        }
    }

    pub fn transaction_root(&self) -> B256 {
        let txs = &self.transactions;
        let n = txs.len();
        if n == 0 {
            return B256::from(mix_in_length(&ZERO_HASHES[TRANSACTIONS_DEPTH as usize], 0));
        }
        // A zero chunk represents an empty transaction before fixed-depth padding.
        let leaves = |tx: &Transaction| tx.len().div_ceil(32).max(1);
        let mut pairs =
            Vec::with_capacity(txs.iter().map(|tx| leaves(tx).next_multiple_of(2)).sum());
        for tx in txs.iter() {
            let start = pairs.len();
            pack_byte_chunks(tx, |chunk| pairs.push(chunk));
            pairs.resize(start + leaves(tx).next_multiple_of(2), ZERO_HASH);
        }
        let mut nodes = vec![ZERO_HASH; pairs.len() / 2];
        let len = pairs.len();
        merkleize_trees(&mut pairs, &mut nodes, len, txs.iter().map(leaves), TRANSACTION_DEPTH);
        for (i, tx) in txs.iter().enumerate() {
            pairs[2 * i] = nodes[i];
            pairs[2 * i + 1] = uint64_chunk(tx.len() as u64);
        }
        hash_concat_many(&mut nodes[..n], &pairs[..2 * n]);
        pairs[..n].copy_from_slice(&nodes[..n]);
        // Each transaction reserved at least two entries, leaving room for an odd-list pad.
        pairs[n] = ZERO_HASH;
        let len = n.next_multiple_of(2);
        merkleize_trees(&mut pairs, &mut nodes, len, iter::once(n), TRANSACTIONS_DEPTH);
        B256::from(mix_in_length(&nodes[0], n))
    }

    pub fn withdrawals_root(&self) -> B256 {
        withdrawals_root(&self.withdrawals)
    }

    pub fn to_lighthouse_fulu_payload(
        &self,
    ) -> Result<lh_types::ExecutionPayloadFulu<MainnetEthSpec>, SszError> {
        Ok(lh_types::ExecutionPayloadFulu {
            parent_hash: self.parent_hash.into(),
            fee_recipient: self.fee_recipient,
            state_root: self.state_root,
            receipts_root: self.receipts_root,
            logs_bloom: convert_bloom_to_lighthouse(&self.logs_bloom),
            prev_randao: self.prev_randao,
            block_number: self.block_number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            extra_data: self.extra_data.to_ssz_type()?,
            base_fee_per_gas: self.base_fee_per_gas,
            block_hash: self.block_hash.into(),
            transactions: convert_transactions_to_lighthouse(&self.transactions)?,
            withdrawals: self.withdrawals.clone(),
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
        })
    }

    /// Converts to the real, progressive-list Gloas execution payload shape used on-chain.
    ///
    /// `block_access_list` is the opaque EIP-7928 list the builder submitted: only
    /// an execution client can produce it, so it travels on the submission.
    pub fn to_lighthouse_gloas_payload(
        &self,
        slot: lh_types::Slot,
        block_access_list: &crate::fields::BlockAccessListBytes,
    ) -> Result<lh_types::ExecutionPayloadGloas<MainnetEthSpec>, SszError> {
        Ok(lh_types::ExecutionPayloadGloas {
            parent_hash: self.parent_hash.into(),
            fee_recipient: self.fee_recipient,
            state_root: self.state_root,
            receipts_root: self.receipts_root,
            logs_bloom: convert_bloom_to_lighthouse(&self.logs_bloom),
            prev_randao: self.prev_randao,
            block_number: self.block_number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            extra_data: self.extra_data.to_ssz_type()?,
            base_fee_per_gas: self.base_fee_per_gas,
            block_hash: self.block_hash.into(),
            transactions: convert_transactions_to_progressive(&self.transactions),
            withdrawals: self.withdrawals.iter().cloned().collect(),
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
            block_access_list: block_access_list.iter().copied().collect(),
            slot_number: slot,
        })
    }
}

pub fn withdrawals_root(withdrawals: &Withdrawals) -> B256 {
    withdrawals_root_of(
        withdrawals.iter().map(|w| (w.index, w.validator_index, &w.address.0.0, w.amount)),
    )
}

/// Computes the bounded SSZ `List[Withdrawal, 16]` root from withdrawal fields.
/// Callers must supply at most 16 withdrawals.
pub(crate) fn withdrawals_root_of<'a>(
    withdrawals: impl Iterator<Item = (u64, u64, &'a [u8; 20], u64)>,
) -> B256 {
    let mut leaves = [[ZERO_HASH; 4]; MAX_WITHDRAWALS_PER_PAYLOAD];
    let mut n = 0;
    for (leaf, (index, validator_index, address, amount)) in leaves.iter_mut().zip(withdrawals) {
        leaf[0] = uint64_chunk(index);
        leaf[1] = uint64_chunk(validator_index);
        leaf[2][..20].copy_from_slice(address);
        leaf[3] = uint64_chunk(amount);
        n += 1;
    }
    if n == 0 {
        return B256::from(mix_in_length(&ZERO_HASHES[WITHDRAWALS_DEPTH as usize], 0));
    }
    let mut nodes = [ZERO_HASH; 2 * MAX_WITHDRAWALS_PER_PAYLOAD];
    // Unused entries supply zero chunks when the withdrawal count is odd.
    let mut pairs = [ZERO_HASH; MAX_WITHDRAWALS_PER_PAYLOAD];
    hash_concat_many(&mut nodes[..2 * n], leaves[..n].as_flattened());
    hash_concat_many(&mut pairs[..n], &nodes[..2 * n]);
    let len = n.next_multiple_of(2);
    merkleize_trees(&mut pairs, &mut nodes, len, iter::once(n), WITHDRAWALS_DEPTH);
    B256::from(mix_in_length(&nodes[0], n))
}

/// Batches independent pairs from adjacent SSZ chunk trees at each level.
/// `depth` must be positive; every count from `leaves` must be in `1..=2^depth`.
/// `pairs[..len]` contains the trees in order, each padded to an even leaf count
/// with `ZERO_HASH`. `nodes` must have room for `len / 2` hashes.
/// Roots are written at the start of `nodes` in tree order, without length mix-ins.
fn merkleize_trees(
    pairs: &mut [merkle::B256],
    nodes: &mut [merkle::B256],
    mut len: usize,
    leaves: impl Iterator<Item = usize> + Clone,
    depth: u32,
) {
    for level in 1..=depth {
        hash_concat_many(&mut nodes[..len / 2], &pairs[..len]);
        if level == depth {
            return;
        }
        let (mut from, mut to) = (0, 0);
        for count in leaves.clone().map(|leaves| leaves.div_ceil(1 << level)) {
            pairs[to..to + count].copy_from_slice(&nodes[from..from + count]);
            from += count;
            to += count;
            if count % 2 == 1 {
                pairs[to] = ZERO_HASHES[level as usize];
                to += 1;
            }
        }
        len = to;
    }
}

impl ForkVersionDecode for ExecutionPayload {
    /// SSZ decode with explicit fork variant. Gloas uses the same bounded-list wire shape as
    /// Fulu here -- this is helix's own builder<->relay representation, not the real,
    /// progressive-list consensus `ExecutionPayloadGloas`; see `to_lighthouse_gloas_payload`.
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork_name: ForkName) -> Result<Self, ssz::DecodeError> {
        let builder_bid = match fork_name {
            ForkName::Altair |
            ForkName::Base |
            ForkName::Bellatrix |
            ForkName::Capella |
            ForkName::Deneb |
            ForkName::Electra |
            ForkName::Heze => {
                return Err(ssz::DecodeError::BytesInvalid(format!(
                    "unsupported fork for ExecutionPayloadHeader: {fork_name}",
                )));
            }
            ForkName::Fulu | ForkName::Gloas => ExecutionPayload::from_ssz_bytes(bytes)?,
        };
        Ok(builder_bid)
    }
}

/// From lighthouse, replacing a few List<u8>
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPayloadHeader {
    pub parent_hash: B256,
    pub fee_recipient: Address,
    pub state_root: B256,
    pub receipts_root: B256,
    pub logs_bloom: Bloom, // replaced
    pub prev_randao: B256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub block_number: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub extra_data: ExtraData, // replaced
    #[serde(with = "serde_utils::quoted_u256")]
    pub base_fee_per_gas: U256,
    pub block_hash: B256,
    pub transactions_root: B256,
    pub withdrawals_root: B256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub blob_gas_used: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub excess_blob_gas: u64,
}

impl ExecutionPayloadHeader {
    /// Since the variable list types are now wrapped in Bytes, we need to validate that the fields
    /// are of the correct length
    pub fn validate_ssz_lengths(&self) -> Result<(), BlockValidationError> {
        if self.extra_data.len() > <MainnetEthSpec as EthSpec>::max_extra_data_bytes() {
            return Err(BlockValidationError::SszError(SszError::InvalidByteCount {
                given: self.extra_data.len(),
                expected: <MainnetEthSpec as EthSpec>::max_extra_data_bytes(),
            }));
        }

        Ok(())
    }

    pub fn to_lighthouse_fulu_header(
        &self,
    ) -> Result<lh_types::ExecutionPayloadHeaderFulu<MainnetEthSpec>, SszError> {
        Ok(lh_types::ExecutionPayloadHeaderFulu {
            parent_hash: self.parent_hash.into(),
            fee_recipient: self.fee_recipient,
            state_root: self.state_root,
            receipts_root: self.receipts_root,
            logs_bloom: convert_bloom_to_lighthouse(&self.logs_bloom),
            prev_randao: self.prev_randao,
            block_number: self.block_number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            extra_data: self.extra_data.to_ssz_type()?,
            base_fee_per_gas: self.base_fee_per_gas,
            block_hash: self.block_hash.into(),
            transactions_root: self.transactions_root,
            withdrawals_root: self.withdrawals_root,
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
        })
    }
}

impl TestRandom for ExecutionPayloadHeader {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            parent_hash: B256::random_for_test(rng),
            fee_recipient: Address::random_for_test(rng),
            state_root: B256::random_for_test(rng),
            receipts_root: B256::random_for_test(rng),
            logs_bloom: Bloom::random(),
            prev_randao: B256::random_for_test(rng),
            block_number: u64::random_for_test(rng),
            gas_limit: u64::random_for_test(rng),
            gas_used: u64::random_for_test(rng),
            timestamp: u64::random_for_test(rng),
            extra_data: ExtraData::default(),
            base_fee_per_gas: U256::random_for_test(rng),
            block_hash: B256::random_for_test(rng),
            transactions_root: B256::random_for_test(rng),
            withdrawals_root: B256::random_for_test(rng),
            blob_gas_used: u64::random_for_test(rng),
            excess_blob_gas: u64::random_for_test(rng),
        }
    }
}

impl From<&ExecutionPayload> for ExecutionPayloadHeader {
    fn from(value: &ExecutionPayload) -> Self {
        let transactions_root = value.transaction_root();
        let withdrawals_root = value.withdrawals_root();
        Self {
            parent_hash: value.parent_hash,
            fee_recipient: value.fee_recipient,
            state_root: value.state_root,
            receipts_root: value.receipts_root,
            logs_bloom: value.logs_bloom,
            prev_randao: value.prev_randao,
            block_number: value.block_number,
            gas_limit: value.gas_limit,
            gas_used: value.gas_used,
            timestamp: value.timestamp,
            extra_data: value.extra_data.clone(),
            base_fee_per_gas: value.base_fee_per_gas,
            block_hash: value.block_hash,
            transactions_root,
            withdrawals_root,
            blob_gas_used: value.blob_gas_used,
            excess_blob_gas: value.excess_blob_gas,
        }
    }
}

#[cfg(test)]
mod tests {
    use lh_types::{ForkName, MainnetEthSpec, Withdrawal};
    use rand::{Rng, RngCore, SeedableRng};
    use rand_xorshift::XorShiftRng;
    use ssz::Encode;
    use tree_hash::TreeHash;

    use super::*;
    use crate::TestRandomSeed;

    #[test]
    fn roots_match_lighthouse() {
        let mut rng = XorShiftRng::from_seed([7; 16]);
        let boundaries = vec![0, 1, 31, 32, 33, 63, 64, 65, 100_000];
        let mainnet_like = (0..300).map(|_| rng.random_range(100..=600)).collect();
        for tx_lens in [vec![], vec![0], vec![1], vec![64, 32], boundaries, mainnet_like] {
            for withdrawals in [0, 1, 15, 16] {
                let mut payload = ExecutionPayload::test_random();
                let txs = tx_lens.iter().map(|&len| {
                    let mut tx = vec![0; len];
                    rng.fill_bytes(&mut tx);
                    Transaction(tx.into())
                });
                payload.transactions = Transactions::new(txs.collect()).unwrap();
                let withdrawals = (0..withdrawals).map(|_| Withdrawal::random_for_test(&mut rng));
                payload.withdrawals = Withdrawals::new(withdrawals.collect()).unwrap();
                let lighthouse = payload.to_lighthouse_fulu_payload().unwrap();

                let case =
                    format!("{} txs, {} withdrawals", tx_lens.len(), payload.withdrawals.len());
                assert_eq!(
                    payload.transaction_root(),
                    lighthouse.transactions.tree_hash_root(),
                    "{case}"
                );
                assert_eq!(
                    payload.withdrawals_root(),
                    lighthouse.withdrawals.tree_hash_root(),
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn test_execution_payload() {
        test_execution_payload_variant(ForkName::Fulu);
    }

    fn test_execution_payload_variant(fork_name: ForkName) {
        type LhExecutionPayload = lh_types::ExecutionPayload<MainnetEthSpec>;

        let our_payload = ExecutionPayload::test_random();
        let ssz_bytes = our_payload.as_ssz_bytes();

        // ssz
        let lh_decode = LhExecutionPayload::from_ssz_bytes_by_fork(&ssz_bytes, fork_name).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_decode.tree_hash_root());

        // serde
        let json_str = serde_json::to_value(&our_payload).unwrap();
        let lh_json_str: LhExecutionPayload = serde_json::from_value(json_str).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_json_str.tree_hash_root());
    }

    #[test]
    fn test_execution_payload_decodes_under_gloas_fork() {
        let our_payload = ExecutionPayload::test_random();
        let ssz_bytes = our_payload.as_ssz_bytes();

        let decoded = ExecutionPayload::from_ssz_bytes_by_fork(&ssz_bytes, ForkName::Gloas)
            .expect("Gloas should decode via the same shape as Fulu");

        assert_eq!(our_payload.tree_hash_root(), decoded.tree_hash_root());
    }

    #[test]
    fn the_gloas_payload_carries_the_submitted_block_access_list() {
        let payload = ExecutionPayload::test_random();
        let bal = crate::fields::BlockAccessListBytes(vec![9u8; 128].into());

        let gloas = payload.to_lighthouse_gloas_payload(lh_types::Slot::new(1), &bal).unwrap();

        // Before this it was always empty, which made the envelope invalid.
        assert_eq!(gloas.block_access_list.len(), 128);
        assert_eq!(gloas.block_access_list.to_vec(), bal.to_vec());
    }

    #[test]
    fn an_empty_block_access_list_still_converts() {
        let payload = ExecutionPayload::test_random();

        let gloas = payload
            .to_lighthouse_gloas_payload(
                lh_types::Slot::new(1),
                &crate::fields::BlockAccessListBytes::default(),
            )
            .unwrap();

        assert!(gloas.block_access_list.is_empty());
    }

    #[test]
    fn to_lighthouse_gloas_payload_preserves_fields() {
        let our_payload = ExecutionPayload::test_random();
        let slot = lh_types::Slot::new(42);

        let bal = crate::fields::BlockAccessListBytes(vec![1u8, 2, 3].into());
        let gloas = our_payload.to_lighthouse_gloas_payload(slot, &bal).unwrap();

        assert_eq!(gloas.parent_hash.0, our_payload.parent_hash);
        assert_eq!(gloas.block_hash.0, our_payload.block_hash);
        assert_eq!(gloas.fee_recipient, our_payload.fee_recipient);
        assert_eq!(gloas.state_root, our_payload.state_root);
        assert_eq!(gloas.receipts_root, our_payload.receipts_root);
        assert_eq!(gloas.prev_randao, our_payload.prev_randao);
        assert_eq!(gloas.block_number, our_payload.block_number);
        assert_eq!(gloas.gas_limit, our_payload.gas_limit);
        assert_eq!(gloas.gas_used, our_payload.gas_used);
        assert_eq!(gloas.timestamp, our_payload.timestamp);
        assert_eq!(gloas.base_fee_per_gas, our_payload.base_fee_per_gas);
        assert_eq!(gloas.blob_gas_used, our_payload.blob_gas_used);
        assert_eq!(gloas.excess_blob_gas, our_payload.excess_blob_gas);
        assert_eq!(gloas.slot_number, slot);

        assert_eq!(gloas.transactions.len(), our_payload.transactions.len());
        for (converted, original) in
            gloas.transactions.as_slice().iter().zip(our_payload.transactions.iter())
        {
            assert_eq!(converted.as_slice(), original.as_ref());
        }

        assert_eq!(gloas.withdrawals.len(), our_payload.withdrawals.len());
        for (converted, original) in
            gloas.withdrawals.as_slice().iter().zip(our_payload.withdrawals.iter())
        {
            assert_eq!(converted, original);
        }
    }

    #[test]
    fn test_execution_payload_header() {
        test_execution_payload_header_variant(ForkName::Fulu);
    }

    fn test_execution_payload_header_variant(fork_name: ForkName) {
        type LhExecutionPayloadHeader = lh_types::ExecutionPayloadHeader<MainnetEthSpec>;

        let our_payload = ExecutionPayloadHeader::test_random();
        let ssz_bytes = our_payload.as_ssz_bytes();

        // ssz
        let lh_decode = LhExecutionPayloadHeader::from_ssz_bytes(&ssz_bytes, fork_name).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_decode.tree_hash_root());

        // serde
        let json_str = serde_json::to_value(&our_payload).unwrap();
        let lh_json_str: LhExecutionPayloadHeader = serde_json::from_value(json_str).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_json_str.tree_hash_root());
    }

    #[test]
    fn test_payload_to_header_conversion() {
        type LhExecutionPayload = lh_types::ExecutionPayload<MainnetEthSpec>;
        type LhExecutionPayloadHeader = lh_types::ExecutionPayloadHeaderFulu<MainnetEthSpec>;

        let our_payload = ExecutionPayload::test_random();
        let ssz_bytes = our_payload.as_ssz_bytes();
        let lh_decode =
            LhExecutionPayload::from_ssz_bytes_by_fork(&ssz_bytes, ForkName::Fulu).unwrap();
        let lh_decode = lh_decode.as_fulu().unwrap();

        let header = our_payload.to_header(None, None);
        let lh_header = LhExecutionPayloadHeader::from(lh_decode);

        assert_eq!(header.tree_hash_root(), lh_header.tree_hash_root());
    }
}
