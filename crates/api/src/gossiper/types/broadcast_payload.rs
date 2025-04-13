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
        let fork_name = proto_params.fork_name.and_then(|s| s.parse::<ForkName>().ok());
        Self {
            execution_payload: decode_ssz_payload_and_blobs(
                &proto_params.execution_payload,
                fork_name,
            )
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
            fork_name: Some(self.execution_payload.execution_payload.fork_name().to_string()),
        }
    }
}

// TODO: pass around getpayload response instead of payload and blobs
fn decode_ssz_payload_and_blobs(
    bytes: &[u8],
    fork_name: Option<ForkName>,
) -> Result<PayloadAndBlobs, ssz::DecodeError> {
    if let Some(fork_name) = fork_name {
        return PayloadAndBlobs::from_ssz_bytes_by_fork(bytes, fork_name);
    }

    if let Ok(payload_and_blobs) = PayloadAndBlobs::from_ssz_bytes_by_fork(bytes, ForkName::Electra)
    {
        Ok(payload_and_blobs)
    } else {
        PayloadAndBlobs::from_ssz_bytes_by_fork(bytes, ForkName::Deneb)
    }
}
