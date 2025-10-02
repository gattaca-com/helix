use helix_types::{ForkName, ForkVersionDecode, SignedBlindedBeaconBlock};
use ssz::Encode;
use uuid::Uuid;

use crate::{gossiper::error::GossipError, grpc};

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
            signed_blinded_beacon_block: SignedBlindedBeaconBlock::as_ssz_bytes(
                &self.signed_blinded_beacon_block,
            ),
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
