use helix_tcp_types::{BidSubmissionResponse, ParseError, Status};
use uuid::Uuid;

use crate::api::builder::error::BuilderApiError;

#[derive(Debug, thiserror::Error)]
pub enum BidSubmissionError {
    #[error("Internal error")]
    InternalError,
    #[error(transparent)]
    BuilderApiError(#[from] BuilderApiError),
    #[error(transparent)]
    ParseError(#[from] ParseError),
}

impl From<&BuilderApiError> for Status {
    fn from(e: &BuilderApiError) -> Self {
        match e {
            BuilderApiError::DatabaseError(_) | BuilderApiError::InternalError => {
                Status::InternalError
            }
            _ => Status::InvalidRequest,
        }
    }
}

impl From<&BidSubmissionError> for Status {
    fn from(e: &BidSubmissionError) -> Self {
        match e {
            BidSubmissionError::InternalError => Status::InternalError,
            BidSubmissionError::BuilderApiError(e) => Status::from(e),
            BidSubmissionError::ParseError(_) => Status::InvalidRequest,
        }
    }
}

pub fn response_from_builder_api_error(
    seq_num: u32,
    request_id: Uuid,
    result: &Result<(), BuilderApiError>,
) -> BidSubmissionResponse {
    match result {
        Ok(()) => BidSubmissionResponse {
            sequence_number: seq_num,
            request_id: request_id.into_bytes(),
            status: Status::Okay,
            error_msg: Default::default(),
        },
        Err(e) => BidSubmissionResponse {
            sequence_number: seq_num,
            request_id: request_id.into_bytes(),
            status: Status::from(e),
            error_msg: e.to_string().into_bytes(),
        },
    }
}

pub fn response_from_bid_submission_error(
    seq_num: Option<u32>,
    request_id: Option<Uuid>,
    e: &BidSubmissionError,
) -> BidSubmissionResponse {
    BidSubmissionResponse {
        sequence_number: seq_num.unwrap_or_default(),
        request_id: request_id.map(Uuid::into_bytes).unwrap_or_default(),
        status: Status::from(e),
        error_msg: e.to_string().into_bytes(),
    }
}
