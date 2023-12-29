pub mod api;
pub mod error;
pub mod simulator;
pub mod tests;
pub mod types;

pub use simulator::*;
pub use types::*;
use crate::grpc;

#[derive(Clone, Default, Debug)]
pub struct SubmitBlockParams {
    pub body_bytes: bytes::Bytes,
    pub is_cancellations_enabled: bool,
    pub is_gzip: bool,
    pub is_ssz: bool,
}

impl SubmitBlockParams {

    pub fn from_proto_submit_block_params(proto_submit_params: grpc::SubmitBlockParams) -> Self {
        Self {
            body_bytes: bytes::Bytes::from(proto_submit_params.body),
            is_cancellations_enabled: proto_submit_params.is_cancellations_enabled,
            is_gzip: proto_submit_params.is_gzip,
            is_ssz: proto_submit_params.is_ssz,
        }
    }
    pub fn to_proto_submit_block_params(&self) -> grpc::SubmitBlockParams {
        grpc::SubmitBlockParams {
            body: self.body_bytes.to_vec(),
            is_cancellations_enabled: self.is_cancellations_enabled,
            is_gzip: self.is_gzip,
            is_ssz: self.is_ssz,
        }
    }
}
