use crate::deneb::BlobSidecars;
use ethereum_consensus::primitives::Root;

/// Struct used in the custom `publish_blobs` beacon chain api.
/// Beacon chain expects this JSON serialised.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PublishBlobsRequest {
    pub blob_sidecars: BlobSidecars,
    pub beacon_root: Root,
}

#[test]
fn test_serde() {
    let blobs = PublishBlobsRequest::default();
    let json = serde_json::to_vec(&blobs).unwrap();

    println!("{:?}", json);
}
