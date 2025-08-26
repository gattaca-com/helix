use alloy_primitives::B256;
use helix_types::BlobSidecars;

/// Struct used in the custom `publish_blobs` beacon chain api.
/// Beacon chain expects this JSON serialised.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishBlobsRequest {
    pub blob_sidecars: BlobSidecars,
    pub beacon_root: B256,
}
