use std::sync::Arc;

use alloy_primitives::{B256, U256};
use serde::Serialize;
use ssz_derive::{Decode, Encode};

use crate::{BlsPublicKeyBytes, ExecutionRequests};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PayloadBidData {
    pub withdrawals_root: B256,
    pub tx_root: Option<B256>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub value: U256,
    pub builder_pubkey: BlsPublicKeyBytes,
}

#[derive(Clone, PartialEq, Debug, Encode, Serialize)]
pub struct PayloadBidDataRef<'a> {
    pub withdrawals_root: &'a B256,
    pub tx_root: &'a Option<B256>,
    pub execution_requests: &'a Arc<ExecutionRequests>,
    pub value: &'a U256,
    pub builder_pubkey: &'a BlsPublicKeyBytes,
}

impl<'a> From<&'a PayloadBidData> for PayloadBidDataRef<'a> {
    fn from(bid_data: &'a PayloadBidData) -> Self {
        Self {
            withdrawals_root: &bid_data.withdrawals_root,
            tx_root: &bid_data.tx_root,
            execution_requests: &bid_data.execution_requests,
            value: &bid_data.value,
            builder_pubkey: &bid_data.builder_pubkey,
        }
    }
}

impl PayloadBidDataRef<'_> {
    pub fn to_owned(&self) -> PayloadBidData {
        let withdrawals_root = *self.withdrawals_root;
        let tx_root = *self.tx_root;
        let execution_requests = self.execution_requests.clone();
        let value = *self.value;
        let builder_pubkey = *self.builder_pubkey;
        PayloadBidData { withdrawals_root, tx_root, execution_requests, value, builder_pubkey }
    }
}
