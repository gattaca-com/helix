use alloy_primitives::{Address, U256};
use helix_common::{Filtering, GetPayloadTrace};
use helix_types::{BlsPublicKeyBytes, PayloadAndBlobs};

pub struct SavePayloadParams {
    pub slot: u64,
    pub builder_pub_key: BlsPublicKeyBytes,
    pub proposer_pub_key: BlsPublicKeyBytes,
    pub value: U256,
    pub proposer_fee_recipient: Address,
    pub payload: PayloadAndBlobs,
    pub latency_trace: GetPayloadTrace,
    pub user_agent: Option<String>,
    pub filtering: Filtering,
}
