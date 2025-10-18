use std::sync::Arc;

use helix_types::{
    BlsPublicKeyBytes, ForkName, ForkVersionDecode, PayloadAndBlobs, SignedBlindedBeaconBlock,
};
use ssz::Encode;
use uuid::Uuid;

use super::grpc::{self};
use crate::gossip::error::GossipError;

#[derive(Debug, Clone)]
pub enum GossipedMessage {
    Payload(Box<BroadcastPayloadParams>),
    GetPayload(Box<BroadcastGetPayloadParams>),
}

#[derive(Clone, Debug)]
pub struct BroadcastGetPayloadParams {
    pub signed_blinded_beacon_block: SignedBlindedBeaconBlock,
    pub request_id: Uuid,
}

impl BroadcastGetPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastGetPayloadParams) -> Result<Self, GossipError> {
        let fork_name = proto_params.fork_name.and_then(|s| s.parse::<ForkName>().ok());
        let request_id = Uuid::from_slice(&proto_params.request_id)?;

        let signed_blinded_beacon_block = decode_ssz_signed_blinded_beacon_block(
            &proto_params.signed_blinded_beacon_block,
            fork_name,
        )
        .map_err(GossipError::SszDecodeError)?;

        Ok(Self { signed_blinded_beacon_block, request_id })
    }

    pub fn to_proto(&self) -> grpc::BroadcastGetPayloadParams {
        grpc::BroadcastGetPayloadParams {
            signed_blinded_beacon_block: self.signed_blinded_beacon_block.as_ssz_bytes(),
            request_id: self.request_id.as_bytes().to_vec(),
            fork_name: Some(self.signed_blinded_beacon_block.fork_name_unchecked().to_string()),
        }
    }
}

// TODO: pass around getpayload response instead of payload and blobs
fn decode_ssz_signed_blinded_beacon_block(
    bytes: &[u8],
    fork_name: Option<ForkName>,
) -> Result<SignedBlindedBeaconBlock, ssz::DecodeError> {
    if let Some(fork_name) = fork_name {
        return SignedBlindedBeaconBlock::from_ssz_bytes_by_fork(bytes, fork_name);
    }

    Err(ssz::DecodeError::NoMatchingVariant)
}

#[derive(Clone, Debug)]
pub struct BroadcastPayloadParams {
    pub execution_payload: Arc<PayloadAndBlobs>,
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKeyBytes,
}

impl BroadcastPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastPayloadParams) -> Result<Self, GossipError> {
        let fork_name = proto_params.fork_name.and_then(|s| s.parse::<ForkName>().ok());

        Ok(Self {
            execution_payload: decode_ssz_payload_and_blobs(
                &proto_params.execution_payload,
                fork_name,
            )
            .map_err(GossipError::SszDecodeError)?,
            slot: proto_params.slot,
            proposer_pub_key: BlsPublicKeyBytes::try_from(proto_params.proposer_pub_key.as_slice())
                .map_err(|_| GossipError::BlsDecodeError)?,
        })
    }

    pub fn to_proto(
        execution_payload: &PayloadAndBlobs,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        current_fork: ForkName,
    ) -> grpc::BroadcastPayloadParams {
        grpc::BroadcastPayloadParams {
            execution_payload: execution_payload.as_ssz_bytes(),
            slot,
            proposer_pub_key: proposer_pub_key.to_vec(),
            fork_name: Some(current_fork.to_string()),
        }
    }
}

// TODO: pass around getpayload response instead of payload and blobs
fn decode_ssz_payload_and_blobs(
    bytes: &[u8],
    fork_name: Option<ForkName>,
) -> Result<Arc<PayloadAndBlobs>, ssz::DecodeError> {
    if let Some(fork_name) = fork_name {
        let payload = PayloadAndBlobs::from_ssz_bytes_by_fork(bytes, fork_name)?;
        return Ok(Arc::new(payload));
    }

    Err(ssz::DecodeError::NoMatchingVariant)
}
