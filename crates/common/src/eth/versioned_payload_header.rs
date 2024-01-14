use ethereum_consensus::{
    types::mainnet::ExecutionPayloadHeader,
    ssz::prelude::*,
};

use crate::{deneb::BlobsBundle, capella};


#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct VersionedExecutionPayloadHeader {
    pub execution_payload_header: ExecutionPayloadHeader,
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Default for VersionedExecutionPayloadHeader {
    fn default() -> Self {
        Self {
            execution_payload_header: ExecutionPayloadHeader::Capella(capella::ExecutionPayloadHeader::default()),
            blobs_bundle: None,
        }
    }
}