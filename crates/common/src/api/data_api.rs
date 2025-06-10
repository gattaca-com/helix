use alloy_primitives::{Address, B256, U256};
use helix_types::{BlsPublicKey, Slot};
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
    pub builder_pubkey: BlsPublicKey,
    pub proposer_pubkey: BlsPublicKey,
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
    pub builder_pubkey: BlsPublicKey,
    pub proposer_pubkey: BlsPublicKey,
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
    pub pubkey: BlsPublicKey,
}
