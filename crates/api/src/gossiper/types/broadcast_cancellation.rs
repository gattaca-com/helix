use helix_common::bid_submission::cancellation::SignedCancellation;
use uuid::Uuid;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastCancellationParams {
    pub signed_cancellation: SignedCancellation,
    pub request_id: Uuid,
}

impl BroadcastCancellationParams {
    pub fn from_proto(proto_params: grpc::BroadcastCancellationParams) -> Self {
        Self {
            signed_cancellation: serde_json::from_slice(&proto_params.signed_cancellation).unwrap(),
            request_id: Uuid::from_slice(&proto_params.request_id).unwrap(),
        }
    }

    pub fn to_proto(&self) -> grpc::BroadcastCancellationParams {
        grpc::BroadcastCancellationParams {
            signed_cancellation: serde_json::to_vec(&self.signed_cancellation).unwrap(),
            request_id: self.request_id.as_bytes().to_vec(),
        }
    }
}
