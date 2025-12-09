use core::fmt;

use alloy_primitives::{Address, B256, Bloom, Bytes};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeSeed, Error, SeqAccess, Visitor},
};
use ssz_derive::{Decode, Encode};

pub type Proof = Vec<Vec<u8>>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum BidAdjustmentData {
    Original(BidAdjData),
    GasUsed(BidAdjDataWithGasUsed),
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum BidAdjustmentDataV2 {
    Original(BidAdjDataV2),
    GasUsed(BidAdjDataV2WithGasUsed),
}

/// Adjustment data compatible with Ultrasound spec.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
pub struct BidAdjData {
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub builder_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub builder_proof: Proof,
    pub fee_recipient_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_recipient_proof: Proof,
    pub fee_payer_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_payer_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_tx_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_receipt_proof: Proof,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
pub struct BidAdjDataWithGasUsed {
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub builder_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub builder_proof: Proof,
    pub fee_recipient_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_recipient_proof: Proof,
    pub fee_payer_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_payer_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_tx_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_receipt_proof: Proof,
    pub placeholder_gas_used: u64,
}

/// Adjustment data compatible with Ultrasound v2 spec.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
pub struct BidAdjDataV2 {
    pub el_transactions_root: B256,
    pub el_withdrawals_root: B256,
    pub builder_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub builder_proof: Proof,
    pub fee_recipient_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_recipient_proof: Proof,
    pub fee_payer_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_payer_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub el_placeholder_transaction_proof: Proof,
    pub cl_placeholder_transaction_proof: Vec<B256>,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_receipt_proof: Proof,
    pub pre_payment_logs_bloom: Bloom,
}

/// Adjustment data compatible with Ultrasound v2 spec.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, Clone)]
pub struct BidAdjDataV2WithGasUsed {
    pub el_transactions_root: B256,
    pub el_withdrawals_root: B256,
    pub builder_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub builder_proof: Proof,
    pub fee_recipient_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_recipient_proof: Proof,
    pub fee_payer_address: Address,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub fee_payer_proof: Proof,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub el_placeholder_transaction_proof: Proof,
    pub cl_placeholder_transaction_proof: Vec<B256>,
    #[serde(deserialize_with = "proof_from_bytes")]
    pub placeholder_receipt_proof: Proof,
    pub pre_payment_logs_bloom: Bloom,
    pub el_placeholder_gas_used: u64,
}

struct BytesOrArray;

impl<'de> DeserializeSeed<'de> for BytesOrArray {
    type Value = Bytes;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Bytes;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("bytes convertable to Proof")
            }

            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(Bytes::copy_from_slice(v))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut v: A) -> Result<Self::Value, A::Error> {
                let mut vals: Vec<u8> = vec![];
                while let Some(val) = v.next_element()? {
                    vals.push(val);
                }
                Ok(vals.into())
            }
        }

        deserializer.deserialize_any(BytesVisitor)
    }
}

pub fn proof_from_bytes<'de, D: Deserializer<'de>>(de: D) -> Result<Proof, D::Error> {
    struct ProofVisitor;
    impl<'de> Visitor<'de> for ProofVisitor {
        type Value = Proof;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("bytes convertable to Proof")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut v: A) -> Result<Self::Value, A::Error> {
            let mut vals: Vec<Bytes> = vec![];
            while let Some(val) = v.next_element_seed(BytesOrArray)? {
                vals.push(val);
            }
            Ok(vals.into_iter().map(|b| b.to_vec()).collect())
        }
    }

    de.deserialize_any(ProofVisitor)
}
