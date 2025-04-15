use std::convert::Infallible;

use helix_common::bid_submission::v3::header_submission_v3::MessageHeader;

pub mod payload;
pub mod tcp;

#[derive(Debug, thiserror::Error)]
pub enum V3Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SSZ error: {0:?}")]
    Ssz(ssz::DecodeError),
    #[error("CBOR error: {0:?}")]
    Cbor(#[from] cbor4ii::serde::DecodeError<Infallible>),
    #[error("JSON error: {0:?}")]
    Json(#[from] serde_json::Error),
    #[error("Unknown message encoding: {0:?}")]
    Unknown(MessageHeader),
    #[error("Builder address error: {0}")]
    BuilderAddressError(String),
    #[error("Payload request error: {0}")]
    PayloadRequestError(#[from] reqwest::Error),
}
