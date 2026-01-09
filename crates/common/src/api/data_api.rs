use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, Slot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct BidFilters {
    pub slot: Option<u64>,
    pub cursor: Option<u64>,
    pub limit: Option<u64>,
    pub block_hash: Option<B256>,
    pub block_number: Option<u64>,
    pub proposer_pubkey: Option<BlsPublicKey>,
    pub builder_pubkey: Option<BlsPublicKey>,
    pub order_by: Option<i8>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ProposerPayloadDeliveredParams {
    pub slot: Option<u64>,
    pub cursor: Option<u64>,
    pub limit: Option<u64>,
    pub block_hash: Option<B256>,
    pub block_number: Option<u64>,
    pub proposer_pubkey: Option<BlsPublicKey>,
    pub builder_pubkey: Option<BlsPublicKey>,
    pub order_by: Option<String>,
}

impl From<&ProposerPayloadDeliveredParams> for BidFilters {
    fn from(value: &ProposerPayloadDeliveredParams) -> Self {
        BidFilters {
            slot: value.slot,
            cursor: value.cursor,
            limit: value.limit,
            block_hash: value.block_hash,
            block_number: value.block_number,
            proposer_pubkey: value.proposer_pubkey.clone(),
            builder_pubkey: value.builder_pubkey.clone(),
            order_by: match value.order_by.as_ref() {
                Some(s) => match s.as_str() {
                    "value" => Some(1),
                    "-value" => Some(-1),
                    _ => None,
                },
                None => None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveredPayloadsResponse {
    pub slot: Slot,
    pub parent_hash: B256,
    pub block_hash: B256,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub proposer_pubkey: BlsPublicKeyBytes,
    pub proposer_fee_recipient: Address,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    #[serde(with = "serde_utils::quoted_u256")]
    pub value: U256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub block_number: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub num_tx: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct BuilderBlocksReceivedParams {
    pub slot: Option<u64>,
    pub block_hash: Option<B256>,
    pub block_number: Option<u64>,
    pub builder_pubkey: Option<BlsPublicKey>,
    pub limit: Option<u64>,
}

impl From<&BuilderBlocksReceivedParams> for BidFilters {
    fn from(value: &BuilderBlocksReceivedParams) -> Self {
        BidFilters {
            slot: value.slot,
            cursor: None,
            limit: value.limit,
            block_hash: value.block_hash,
            block_number: value.block_number,
            proposer_pubkey: None,
            builder_pubkey: value.builder_pubkey.clone(),
            order_by: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedBlocksResponse {
    pub slot: Slot,
    pub parent_hash: B256,
    pub block_hash: B256,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub proposer_pubkey: BlsPublicKeyBytes,
    pub proposer_fee_recipient: Address,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    #[serde(with = "serde_utils::quoted_u256")]
    pub value: U256,
    #[serde(with = "serde_utils::quoted_u64")]
    pub block_number: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub num_tx: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidatorRegistrationParams {
    pub pubkey: BlsPublicKeyBytes,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DataAdjustmentsResponse {
    pub builder_pubkey: BlsPublicKeyBytes,
    pub block_number: u64,
    #[serde(with = "serde_utils::quoted_u256")]
    pub delta: U256,
    pub submitted_block_hash: B256,
    pub submitted_received_at: DateTime<Utc>,
    #[serde(with = "serde_utils::quoted_u256")]
    pub submitted_value: U256,
    pub adjusted_block_hash: B256,
    #[serde(with = "serde_utils::quoted_u256")]
    pub adjusted_value: U256,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct DataAdjustmentsParams {
    pub slot: Slot,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ProposerHeaderDeliveredParams {
    pub slot: Option<u64>,
    pub cursor: Option<u64>,
    pub block_hash: Option<B256>,
    pub block_number: Option<u64>,
    pub builder_pubkey: Option<BlsPublicKey>,
    pub proposer_pubkey: Option<BlsPublicKey>,
    pub limit: Option<u64>,
    pub order_by: Option<String>,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct MergedBlockParams {
    pub slot: Slot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposerHeaderDeliveredResponse {
    pub slot: Option<Slot>,
    pub parent_hash: Option<B256>,
    pub block_hash: Option<B256>,
    pub proposer_pubkey: Option<BlsPublicKeyBytes>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "quoted_u256_opt")]
    pub value: Option<U256>,
}

mod quoted_u256_opt {
    use alloy_primitives::U256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<U256>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(v) => serde_utils::quoted_u256::serialize(v, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|s| U256::from_str_radix(&s, 10).map_err(serde::de::Error::custom))
            .transpose()
    }
}
