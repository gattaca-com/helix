use helix_tcp_types::{BidSubmissionResponse, ParseError, SeqNum, Status};

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
    request_id: SeqNum,
    result: &Result<(), BuilderApiError>,
) -> BidSubmissionResponse {
    match result {
        Ok(()) => BidSubmissionResponse {
            sequence_number: request_id,
            status: Status::Okay,
            error_msg: Default::default(),
        },
        Err(e) => BidSubmissionResponse {
            sequence_number: request_id,
            status: Status::from(e),
            error_msg: e.to_string().into_bytes(),
        },
    }
}

pub fn response_from_bid_submission_error(
    request_id: &Option<SeqNum>,
    e: &BidSubmissionError,
) -> BidSubmissionResponse {
    BidSubmissionResponse {
        sequence_number: request_id.unwrap_or_default(),
        status: Status::from(e),
        error_msg: e.to_string().into_bytes(),
    }
}
