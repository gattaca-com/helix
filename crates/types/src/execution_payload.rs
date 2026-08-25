use alloy_primitives::{Address, B256, U256};
use lh_types::{EthSpec, ForkName, ForkVersionDecode, MainnetEthSpec};
use serde::{Deserialize, Serialize};
use ssz::Decode;
use ssz_derive::{Decode, Encode};
use tree_hash::TreeHash;
use tree_hash_derive::TreeHash;

use crate::{
    BlockValidationError, SszError, TestRandom, convert_bloom_to_lighthouse,
    convert_transactions_to_lighthouse, convert_transactions_to_progressive,
    fields::{Bloom, ExtraData, Transactions, Withdrawals},
};

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
        self.transactions.tree_hash_root()
    }

    pub fn withdrawals_root(&self) -> B256 {
        self.withdrawals.tree_hash_root()
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
    /// `block_access_list` is left empty -- TODO(gloas): populate once EIP-7928 block-access-list
    /// tracking exists.
    pub fn to_lighthouse_gloas_payload(
        &self,
        slot: lh_types::Slot,
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
            block_access_list: Default::default(),
            slot_number: slot,
        })
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
        let transactions_root = value.transactions.tree_hash_root();
        let withdrawals_root = value.withdrawals.tree_hash_root();
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
    use lh_types::{ForkName, MainnetEthSpec};
    use ssz::Encode;
    use tree_hash::TreeHash;

    use super::*;
    use crate::TestRandomSeed;

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
    fn to_lighthouse_gloas_payload_preserves_fields() {
        let our_payload = ExecutionPayload::test_random();
        let slot = lh_types::Slot::new(42);

        let gloas = our_payload.to_lighthouse_gloas_payload(slot).unwrap();

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
        assert!(gloas.block_access_list.is_empty());

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
