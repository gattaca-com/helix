use ethereum_consensus::{ssz::prelude::*, types::mainnet::ExecutionPayload};

use crate::{capella, deneb::BlobsBundle};

#[derive(Debug, Clone, Serializable, HashTreeRoot, serde::Serialize, serde::Deserialize)]
pub struct PayloadAndBlobs {
    pub execution_payload: ExecutionPayload,
    pub blobs_bundle: Option<BlobsBundle>,
}

impl Default for PayloadAndBlobs {
    fn default() -> Self {
        Self { execution_payload: ExecutionPayload::Capella(capella::ExecutionPayload::default()), blobs_bundle: None }
    }
}
