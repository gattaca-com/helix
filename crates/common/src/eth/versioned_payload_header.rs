use ethereum_consensus::{ssz::prelude::*, types::mainnet::ExecutionPayloadHeader};

use crate::{capella, deneb::BlobsBundle};

#[derive(Debug, Clone, Serializable, HashTreeRoot, serde::Serialize, serde::Deserialize)]
pub struct VersionedExecutionPayloadHeader {
    pub execution_payload_header: ExecutionPayloadHeader,
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Default for VersionedExecutionPayloadHeader {
    fn default() -> Self {
        Self { execution_payload_header: ExecutionPayloadHeader::Capella(capella::ExecutionPayloadHeader::default()), blobs_bundle: None }
    }
}
