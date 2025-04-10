use helix_types::{BlsPublicKey, ForkName, ForkVersionDecode, PayloadAndBlobs};
use ssz::Encode;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastPayloadParams {
    pub execution_payload: PayloadAndBlobs,
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKey,
}

impl BroadcastPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastPayloadParams) -> Self {
        // TODO: double check this logic
        Self {
            execution_payload: decode_ssz_payload_and_blobs(&proto_params.execution_payload)
                .unwrap(),
            slot: proto_params.slot,
            proposer_pub_key: BlsPublicKey::deserialize(proto_params.proposer_pub_key.as_slice())
                .unwrap(),
        }
    }
    pub fn to_proto(&self) -> grpc::BroadcastPayloadParams {
        grpc::BroadcastPayloadParams {
            execution_payload: self.execution_payload.as_ssz_bytes(),
            slot: self.slot,
            proposer_pub_key: self.proposer_pub_key.serialize().to_vec(),
        }
    }
}

// TODO: pass around getpayload response instead of payload and blobs
fn decode_ssz_payload_and_blobs(
    payload_and_blobs: &[u8],
) -> Result<PayloadAndBlobs, ssz::DecodeError> {
    if let Ok(payload_and_blobs) =
        PayloadAndBlobs::from_ssz_bytes_by_fork(payload_and_blobs, ForkName::Electra)
    {
        Ok(payload_and_blobs)
    } else {
        PayloadAndBlobs::from_ssz_bytes_by_fork(payload_and_blobs, ForkName::Deneb)
    }
}
