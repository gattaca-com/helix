use ethereum_consensus::{
    types::mainnet::ExecutionPayload,
    ssz::prelude::*,
};

use crate::{deneb::BlobsBundle, capella};


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct PayloadAndBlobs {
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Default for PayloadAndBlobs {
    fn default() -> Self {
        Self {
            execution_payload: ExecutionPayload::Capella(capella::ExecutionPayload::default()),
            blobs_bundle: None,
        }
    }
}